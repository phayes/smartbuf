use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded as channel};
use std::convert::TryFrom;
use std::io::{self, Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::thread::{self, JoinHandle};

const DEFAULT_BUF_SIZE: usize = 8 * 1024;
const DEFAULT_QUEUE_LEN: usize = 2;

#[derive(Debug)]
struct Buffer {
    data: Box<[u8]>,
    start_pos: u64,  // Absolute position where this buffer starts
    end: usize,      // Valid data end in buffer
    generation: u64, // Generation identifier for this buffer
}

impl Buffer {
    fn new(size: usize, start_pos: u64) -> Buffer {
        assert!(size > 0);
        Buffer {
            data: vec![0; size].into_boxed_slice(),
            start_pos,
            end: 0,
            generation: 0,
        }
    }

    fn refill<R: Read>(&mut self, mut reader: R) -> io::Result<()> {
        let mut n_read = 0;
        let mut buf = &mut *self.data;
        self.end = 0;

        while !buf.is_empty() {
            match reader.read(buf) {
                Ok(n) => {
                    if n == 0 {
                        // EOF
                        break;
                    }
                    let tmp = buf;
                    buf = &mut tmp[n..];
                    n_read += n;
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
                    // Continue on interrupted, will be retried
                    break;
                }
                Err(e) => return Err(e),
            };
        }

        self.end = n_read;
        Ok(())
    }
}

#[derive(Debug)]
enum Command {
    Seek { generation: u64, pos: SeekFrom },
    Stop,
}

/// A buffered reader with background thread pre-fetching and seek support.
///
/// `SmartBuf` wraps a `Read + Seek` implementation and provides:
/// - Off-thread pre-fetch buffering for improved read performance
/// - Full seek support, optimizing for seeks within buffered data
///
/// # Examples
///
/// ```
/// use smartbuf::SmartBuf;
/// use std::io::{Read, Seek, SeekFrom, Cursor};
///
/// let data = b"Hello, world!";
/// let cursor = Cursor::new(data);
/// let mut reader = SmartBuf::new(cursor);
///
/// let mut buf = vec![0; 5];
/// reader.read(&mut buf).unwrap();
/// assert_eq!(&buf, b"Hello");
///
/// reader.seek(SeekFrom::Start(0)).unwrap();
/// let mut buf = vec![0; 5];
/// reader.read(&mut buf).unwrap();
/// assert_eq!(&buf, b"Hello");
/// ```
pub struct SmartBuf<R: Read + Seek + Send + 'static> {
    // Channels for communication with background thread
    cmd_send: Option<Sender<Command>>,
    buf_recv: Receiver<io::Result<Buffer>>,
    buf_send: Sender<Option<Buffer>>,

    // Current buffer state
    buffer: Option<Buffer>,
    buf_pos: usize, // Position within current buffer
    position: u64,  // Logical position in stream
    current_generation: u64,
    next_generation: u64,

    // Configuration
    bufsize: usize,

    // Thread handle for cleanup
    handle: Option<JoinHandle<()>>,

    // Phantom data to track type parameter
    _phantom: PhantomData<R>,
}

impl<R: Read + Seek + Send + 'static> SmartBuf<R> {
    /// Creates a new `SmartBuf` with default buffer size (8KB) and queue length (2).
    pub fn new(reader: R) -> Self {
        Self::with_capacity(DEFAULT_BUF_SIZE, DEFAULT_QUEUE_LEN, reader)
    }

    /// Creates a new `SmartBuf` with the specified buffer size and queue length.
    ///
    /// `bufsize` is the size of each buffer chunk in bytes.
    /// `queuelen` is the number of buffers to keep in the queue (must be >= 1).
    pub fn with_capacity(bufsize: usize, queuelen: usize, reader: R) -> Self {
        assert!(bufsize > 0);
        assert!(queuelen >= 1);

        let (cmd_send, cmd_recv) = channel();
        let (full_send, full_recv) = channel::<io::Result<Buffer>>();
        let (empty_send, empty_recv) = channel::<Option<Buffer>>();

        // Pre-allocate buffers
        for i in 0..queuelen {
            let buffer = Buffer::new(bufsize, (i * bufsize) as u64);
            empty_send.send(Some(buffer)).ok();
        }

        let mut background_reader = BackgroundReader::new(empty_recv, full_send, cmd_recv, bufsize);

        let handle = thread::spawn(move || {
            background_reader.serve(reader);
        });

        SmartBuf {
            cmd_send: Some(cmd_send),
            buf_recv: full_recv,
            buf_send: empty_send,
            buffer: None,
            buf_pos: 0,
            position: 0,
            current_generation: 0,
            next_generation: 1,
            bufsize,
            handle: Some(handle),
            _phantom: PhantomData,
        }
    }

    /// Returns the current absolute position in the stream.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Returns the buffer size.
    pub fn buffer_size(&self) -> usize {
        self.bufsize
    }

    fn return_current_buffer(&mut self) {
        if let Some(buf) = self.buffer.take() {
            self.buf_send.send(Some(buf)).ok();
        }
        self.buf_pos = 0;
    }

    fn get_next_buffer(&mut self) -> io::Result<()> {
        self.return_current_buffer();

        loop {
            match self.buf_recv.recv() {
                Ok(Ok(buffer)) => {
                    if buffer.generation != self.current_generation {
                        self.buf_send.send(Some(buffer)).ok();
                        continue;
                    }

                    self.position = buffer.start_pos;
                    self.buf_pos = 0;
                    self.buffer = Some(buffer);
                    return Ok(());
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "background thread terminated",
                    ));
                }
            }
        }
    }

    fn ensure_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_none() {
            self.get_next_buffer()?;
        }
        Ok(())
    }

    fn advance_buffer(&mut self) -> io::Result<()> {
        self.return_current_buffer();
        self.get_next_buffer()
    }
}

impl<R: Read + Seek + Send + 'static> Read for SmartBuf<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut total_read = 0;

        while total_read < buf.len() {
            self.ensure_buffer()?;

            let current = match self.buffer.as_ref() {
                Some(b) => b,
                None => break,
            };

            if self.buf_pos >= current.end {
                if current.end == 0 {
                    break;
                }
                self.advance_buffer()?;
                continue;
            }

            let available = current.end - self.buf_pos;
            let to_read = std::cmp::min(available, buf.len() - total_read);
            let start = self.buf_pos;
            let end = start + to_read;

            buf[total_read..total_read + to_read].copy_from_slice(&current.data[start..end]);

            self.buf_pos += to_read;
            self.position = current.start_pos + self.buf_pos as u64;
            total_read += to_read;

            if self.buf_pos >= current.end {
                self.advance_buffer()?;
            }
        }

        Ok(total_read)
    }
}

impl<R: Read + Seek + Send + 'static> Seek for SmartBuf<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (explicit_target, command_pos) = match pos {
            SeekFrom::Start(n) => (Some(n), SeekFrom::Start(n)),
            SeekFrom::Current(delta) => {
                let base = self.position as i128;
                let target = base + delta as i128;
                if target < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "seek before start",
                    ));
                }
                let target = target as u64;
                (Some(target), SeekFrom::Start(target))
            }
            SeekFrom::End(offset) => (None, SeekFrom::End(offset)),
        };

        if let Some(target_pos) = explicit_target {
            if let Some(ref buffer) = self.buffer {
                if target_pos >= buffer.start_pos {
                    let offset = target_pos - buffer.start_pos;
                    if let Ok(offset_usize) = usize::try_from(offset) {
                        if offset_usize <= buffer.end {
                            self.buf_pos = offset_usize;
                            self.position = target_pos;
                            return Ok(target_pos);
                        }
                    }
                }
            }
        }

        let generation = self.next_generation;
        self.next_generation += 1;
        self.current_generation = generation;
        self.return_current_buffer();
        self.buffer = None;
        self.buf_pos = 0;

        let sender = self.cmd_send.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "background thread terminated")
        })?;

        sender
            .send(Command::Seek {
                generation,
                pos: command_pos,
            })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "background thread terminated")
            })?;

        self.get_next_buffer()?;

        let position = if let Some(target_pos) = explicit_target {
            let buffer = self.buffer.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "missing buffer after seek")
            })?;

            let offset = target_pos
                .checked_sub(buffer.start_pos)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before buffer"))?;
            let offset_usize = usize::try_from(offset).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "seek offset too large")
            })?;
            if offset_usize > buffer.end {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "background reader returned buffer not containing target position",
                ));
            }
            self.buf_pos = offset_usize;
            buffer.start_pos + offset
        } else {
            let buffer = self.buffer.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "missing buffer after seek")
            })?;
            buffer.start_pos
        };

        self.position = position;
        Ok(position)
    }
}

impl<R: Read + Seek + Send + 'static> Drop for SmartBuf<R> {
    fn drop(&mut self) {
        self.return_current_buffer();
        // Send stop command to background thread
        if let Some(sender) = self.cmd_send.take() {
            sender.send(Command::Stop).ok();
        }
        // Wait for background thread to finish
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct BackgroundReader {
    empty_recv: Receiver<Option<Buffer>>,
    full_send: Sender<io::Result<Buffer>>,
    cmd_recv: Receiver<Command>,
    current_generation: u64,
}

impl BackgroundReader {
    fn new(
        empty_recv: Receiver<Option<Buffer>>,
        full_send: Sender<io::Result<Buffer>>,
        cmd_recv: Receiver<Command>,
        _bufsize: usize,
    ) -> Self {
        BackgroundReader {
            empty_recv,
            full_send,
            cmd_recv,
            current_generation: 0,
        }
    }

    fn serve<R: Read + Seek>(&mut self, mut reader: R) {
        let mut current_pos = 0u64;
        let mut pending_seek = false;

        loop {
            // Process any pending commands
            while let Ok(cmd) = self.cmd_recv.try_recv() {
                match cmd {
                    Command::Seek { generation, pos } => match reader.seek(pos) {
                        Ok(new_pos) => {
                            current_pos = new_pos;
                            self.current_generation = generation;
                            pending_seek = true;
                        }
                        Err(e) => {
                            self.full_send.send(Err(e)).ok();
                            return;
                        }
                    },
                    Command::Stop => return,
                }
            }

            let buffer_result = if pending_seek {
                pending_seek = false;
                self.empty_recv.recv()
            } else {
                match self.empty_recv.try_recv() {
                    Ok(b) => Ok(b),
                    Err(TryRecvError::Empty) => {
                        thread::yield_now();
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => return,
                }
            };

            match buffer_result {
                Ok(Some(mut buffer)) => {
                    buffer.start_pos = current_pos;
                    buffer.generation = self.current_generation;
                    match buffer.refill(&mut reader) {
                        Ok(()) => {
                            current_pos += buffer.end as u64;
                            if self.full_send.send(Ok(buffer)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let should_break = e.kind() != io::ErrorKind::Interrupted;
                            if self.full_send.send(Err(e)).is_err() {
                                return;
                            }
                            if should_break {
                                return;
                            }
                        }
                    }
                }
                Ok(None) => return,
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn basic_read() {
        let data = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(4, 2, cursor);

        let mut buf = vec![0; 5];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], &data[..5]);
    }

    #[test]
    fn seek_within_buffer() {
        let data: Vec<u8> = (0..20).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read some data to fill buffer
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();

        // Seek back within the buffer
        reader.seek(SeekFrom::Start(2)).unwrap();

        let mut buf = vec![0; 3];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[2, 3, 4]);
    }

    #[test]
    fn seek_outside_buffer() {
        let data: Vec<u8> = (0..30).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read initial data
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();

        // Seek to position outside current buffer
        reader.seek(SeekFrom::Start(20)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[20, 21, 22, 23, 24]);
    }

    #[test]
    fn seek_current_forward() {
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        reader.seek(SeekFrom::Current(5)).unwrap();
        assert_eq!(reader.position(), 5);

        let mut buf = vec![0; 3];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[5, 6, 7]);
    }

    #[test]
    fn seek_current_backward() {
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read forward
        let mut buf = vec![0; 10];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 10);

        // Seek backward within buffer
        reader.seek(SeekFrom::Current(-5)).unwrap();
        assert_eq!(reader.position(), 5);

        let mut buf = vec![0; 3];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[5, 6, 7]);
    }

    #[test]
    fn read_to_eof() {
        let data: Vec<u8> = (0..25).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read first buffer
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf[..n], &data[..10]);

        // Read second buffer
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf[..n], &data[10..20]);

        // Read past EOF
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], &data[20..25]);

        // Read again past EOF should return 0
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn small_capacity() {
        let data: Vec<u8> = (0..8).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(2, 2, cursor);

        let mut buf = vec![0; 3];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], &data[..3]);

        let mut buf = vec![0; 2];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..n], &data[3..5]);

        let mut buf = vec![0; 1];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 1);
        assert_eq!(&buf[..n], &data[5..6]);
    }

    #[test]
    fn seek_start_multiple() {
        let data: Vec<u8> = (0..20).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to middle
        reader.seek(SeekFrom::Start(5)).unwrap();
        let mut buf = vec![0; 4];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[5, 6, 7, 8]);

        // Seek back to start
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0; 4];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[0, 1, 2, 3]);

        // Seek forward again
        reader.seek(SeekFrom::Start(15)).unwrap();
        let mut buf = vec![0; 4];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[15, 16, 17, 18]);

        // Seek back to start again
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0; 4];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[0, 1, 2, 3]);
    }

    #[test]
    fn seek_end() {
        let data: Vec<u8> = (0..17).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(5, 2, cursor);

        // Seek from end
        reader.seek(SeekFrom::End(-6)).unwrap();
        let mut buf = vec![0; 8];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 6);
        assert_eq!(&buf[..n], &data[11..17]);

        // Seek to end
        reader.seek(SeekFrom::End(0)).unwrap();
        let mut buf = vec![0; 8];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn read_empty_buffer() {
        let data: Vec<u8> = (0..10).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(5, 2, cursor);

        let mut buf = vec![0; 0];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn seek_beyond_buffer_boundary() {
        let data: Vec<u8> = (0..30).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read to fill buffer starting at 0
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 5);

        // Seek backward beyond buffer start (should trigger background seek)
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0; 3];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[0, 1, 2]);

        // Seek forward beyond current buffer
        reader.seek(SeekFrom::Start(25)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[25, 26, 27, 28, 29]);
    }

    #[test]
    fn large_read_spans_multiple_buffers() {
        let data: Vec<u8> = (0..100).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 3, cursor);

        // Read more than one buffer size
        let mut buf = vec![0; 25];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 25);
        assert_eq!(&buf[..n], &data[..25]);
        assert_eq!(reader.position(), 25);
    }

    #[test]
    fn multiple_sequential_seeks() {
        let data: Vec<u8> = (0..50).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Sequence of seeks and reads
        reader.seek(SeekFrom::Start(10)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[10, 11, 12, 13, 14]);

        reader.seek(SeekFrom::Start(20)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[20, 21, 22, 23, 24]);

        reader.seek(SeekFrom::Start(5)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[5, 6, 7, 8, 9]);

        reader.seek(SeekFrom::Start(40)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[40, 41, 42, 43, 44]);
    }

    #[test]
    fn seek_current_large_forward() {
        let data: Vec<u8> = (0..50).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Initial read
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 5);

        // Seek forward beyond buffer
        reader.seek(SeekFrom::Current(20)).unwrap();
        assert_eq!(reader.position(), 25);

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[25, 26, 27, 28, 29]);
    }

    #[test]
    fn seek_current_large_backward() {
        let data: Vec<u8> = (0..50).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read forward
        reader.seek(SeekFrom::Start(30)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 35);

        // Seek backward beyond buffer (should trigger background seek)
        reader.seek(SeekFrom::Current(-30)).unwrap();
        assert_eq!(reader.position(), 5);

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[5, 6, 7, 8, 9]);
    }

    #[test]
    fn position_tracking() {
        let data: Vec<u8> = (0..30).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        assert_eq!(reader.position(), 0);

        reader.seek(SeekFrom::Start(10)).unwrap();
        assert_eq!(reader.position(), 10);

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 15);

        reader.seek(SeekFrom::Current(-3)).unwrap();
        assert_eq!(reader.position(), 12);

        let mut buf = vec![0; 2];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 14);
    }

    #[test]
    fn read_at_buffer_boundary() {
        let data: Vec<u8> = (0..30).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read exactly one buffer
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf, &data[..10]);

        // Read exactly one more buffer
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf, &data[10..20]);
    }

    #[test]
    fn seek_to_same_position() {
        let data: Vec<u8> = (0..20).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        reader.seek(SeekFrom::Start(5)).unwrap();
        assert_eq!(reader.position(), 5);

        // Seek to same position
        reader.seek(SeekFrom::Start(5)).unwrap();
        assert_eq!(reader.position(), 5);

        let mut buf = vec![0; 3];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[5, 6, 7]);
    }

    #[test]
    fn read_to_end() {
        let data: Vec<u8> = (0..100).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(20, 2, cursor);

        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn read_exact() {
        let data: Vec<u8> = (0..50).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        let mut buf = vec![0; 30];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[..30]);
        assert_eq!(reader.position(), 30);
    }

    #[test]
    fn read_exact_partial() {
        let data: Vec<u8> = (0..15).collect();
        let cursor: Cursor<Vec<u8>> = Cursor::new(data);
        let mut reader = SmartBuf::with_capacity(5, 2, cursor);

        let mut buf = vec![0; 10];
        reader.read_exact(&mut buf).unwrap();

        // Should fail on second read_exact since we've exhausted data
        let mut buf2 = vec![0; 10];
        assert!(reader.read_exact(&mut buf2).is_err());
    }

    #[test]
    fn various_buffer_sizes() {
        let text = b"The quick brown fox jumps over the lazy dog";
        let n = text.len() + 3;

        // Test various combinations of buffer sizes and queue lengths
        for channel_bufsize in (1..n.min(20)).step_by(3) {
            for queuelen in (1..n.min(10)).step_by(2) {
                let data = text.to_vec();
                let cursor = Cursor::new(data.clone());
                let mut reader = SmartBuf::with_capacity(channel_bufsize, queuelen, cursor);

                let mut result = Vec::new();
                reader.read_to_end(&mut result).unwrap();
                assert_eq!(
                    result, data,
                    "Failed at bufsize: {}, queuelen: {}",
                    channel_bufsize, queuelen
                );
            }
        }
    }

    #[test]
    fn seek_start_comprehensive() {
        let data: Vec<u8> = (0..17).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to position 3 and read
        reader.seek(SeekFrom::Start(3)).unwrap();
        let mut buf = [0; 8];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[3, 4, 5, 6, 7, 8, 9, 10]);

        // Seek back to start
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0; 8];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[0, 1, 2, 3, 4, 5, 6, 7]);

        // Seek to near end
        reader.seek(SeekFrom::Start(13)).unwrap();
        let mut buf = [0; 8];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..n], &[13, 14, 15, 16]);

        // Seek back to start again
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0; 8];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn seek_current_comprehensive() {
        let data: Vec<u8> = (0..17).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(20, 2, cursor);

        // Seek forward from start
        reader.seek(SeekFrom::Current(2)).unwrap();
        let mut buf = [0; 8];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[2, 3, 4, 5, 6, 7, 8, 9]);

        // Seek forward again
        reader.seek(SeekFrom::Current(6)).unwrap();
        let mut buf = [0; 8];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 1);
        assert_eq!(&buf[..n], &[16]);
    }

    #[test]
    fn seek_current_negative_comprehensive() {
        let data: Vec<u8> = (0..17).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(3, 2, cursor);

        // Read forward first
        reader.seek(SeekFrom::Current(4)).unwrap();
        let mut buf = [0; 4];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[4, 5, 6, 7]);

        // Seek backward within buffer
        reader.seek(SeekFrom::Current(-2)).unwrap();
        let mut buf = [0; 4];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[6, 7, 8, 9]);
    }

    #[test]
    fn multiple_small_reads() {
        let data: Vec<u8> = (0..50).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read in small chunks
        for i in 0..10 {
            let mut buf = [0; 5];
            let n = reader.read(&mut buf).unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, &data[i * 5..(i + 1) * 5]);
        }
    }

    #[test]
    fn read_across_buffer_boundaries() {
        let data: Vec<u8> = (0..30).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read that spans exactly 2 buffers
        let mut buf = vec![0; 20];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 20);
        assert_eq!(&buf[..n], &data[..20]);

        // Next read should get remaining data
        let mut buf = vec![0; 20];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf[..n], &data[20..30]);
    }

    #[test]
    fn seek_and_read_sequence() {
        let data: Vec<u8> = (0..40).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(8, 2, cursor);

        // Read, seek, read sequence
        let mut buf = [0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[0, 1, 2, 3, 4]);

        reader.seek(SeekFrom::Start(20)).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[20, 21, 22, 23, 24]);

        reader.seek(SeekFrom::Start(10)).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[10, 11, 12, 13, 14]);

        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[0, 1, 2, 3, 4]);
    }

    #[test]
    fn default_constructor() {
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::new(cursor);

        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[..10], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn buffer_size_accessor() {
        let data: Vec<u8> = (0..10).collect();
        let cursor = Cursor::new(data);
        let reader = SmartBuf::with_capacity(123, 2, cursor);
        assert_eq!(reader.buffer_size(), 123);
    }

    #[test]
    fn read_after_seek_to_end() {
        let data: Vec<u8> = (0..10).collect();
        let cursor = Cursor::new(data);
        let mut reader = SmartBuf::with_capacity(5, 2, cursor);

        // Seek to end
        reader.seek(SeekFrom::End(0)).unwrap();
        let mut buf = [0; 5];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn seek_from_end_negative() {
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(5, 2, cursor);

        // Seek from end with negative offset
        reader.seek(SeekFrom::End(-5)).unwrap();
        let mut buf = [0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], &[15, 16, 17, 18, 19]);
    }

    #[test]
    fn seek_current_zero() {
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read some data
        let mut buf = [0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 5);

        // Seek current by 0 (should be no-op)
        reader.seek(SeekFrom::Current(0)).unwrap();
        assert_eq!(reader.position(), 5);

        // Should continue reading from same position
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf, &[5, 6, 7, 8, 9]);
    }

    #[test]
    fn large_queue_length() {
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 10, cursor);

        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn seek_to_very_end_then_read() {
        let data: Vec<u8> = (0..25).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to end
        reader.seek(SeekFrom::End(0)).unwrap();

        // Try to read - should get 0 bytes
        let mut buf = [0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);

        // Seek back a bit
        reader.seek(SeekFrom::End(-5)).unwrap();
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], &[20, 21, 22, 23, 24]);
    }

    #[test]
    fn deadlock_test_queue_length_one() {
        // Test with minimal buffer pool
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 1, cursor);

        // Read through multiple buffers with queue length of 1
        // This exercises the buffer return/acquire cycle
        for _ in 0..10 {
            let mut buf = vec![0; 10];
            let n = reader.read(&mut buf).unwrap();
            assert!(n > 0);
        }
    }

    #[test]
    fn deadlock_test_rapid_successive_seeks() {
        // Test rapid successive seeks that might cause command queue issues
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Rapid seeks without waiting for each to complete
        for i in 0..20 {
            let pos = (i * 5) % 100;
            reader.seek(SeekFrom::Start(pos)).unwrap();
            let mut buf = vec![0; 3];
            let n = reader.read(&mut buf).unwrap();
            assert!(n > 0);
            assert_eq!(buf[0], pos as u8);
        }
    }

    #[test]
    fn deadlock_test_seek_while_reading() {
        // Test seeks interspersed with reads
        let data: Vec<u8> = (0..200).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read some data
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[0, 1, 2, 3, 4]);

        // Seek while we have a buffer
        reader.seek(SeekFrom::Start(50)).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[50, 51, 52, 53, 54]);

        // Read more, then seek again
        reader.read(&mut buf).unwrap();
        reader.seek(SeekFrom::Start(100)).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[100, 101, 102, 103, 104]);
    }

    #[test]
    fn deadlock_test_multiple_seeks_no_read() {
        // Test multiple seeks in a row without reading
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Chain seeks without reading
        reader.seek(SeekFrom::Start(10)).unwrap();
        reader.seek(SeekFrom::Start(20)).unwrap();
        reader.seek(SeekFrom::Start(30)).unwrap();
        reader.seek(SeekFrom::Start(40)).unwrap();
        reader.seek(SeekFrom::Start(50)).unwrap();

        // Now read to verify we're at the right position
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[50, 51, 52, 53, 54]);
    }

    #[test]
    fn deadlock_test_seek_current_rapid() {
        // Test rapid SeekFrom::Current operations
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Rapid SeekFrom::Current operations
        reader.seek(SeekFrom::Current(10)).unwrap();
        reader.seek(SeekFrom::Current(10)).unwrap();
        reader.seek(SeekFrom::Current(10)).unwrap();
        reader.seek(SeekFrom::Current(-10)).unwrap();
        reader.seek(SeekFrom::Current(5)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[25, 26, 27, 28, 29]);
    }

    #[test]
    fn deadlock_test_seek_end_rapid() {
        // Test rapid SeekFrom::End operations
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Multiple SeekFrom::End operations
        reader.seek(SeekFrom::End(-10)).unwrap();
        reader.seek(SeekFrom::End(-20)).unwrap();
        reader.seek(SeekFrom::End(-5)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[95, 96, 97, 98, 99]);
    }

    #[test]
    fn deadlock_test_interleaved_read_seek() {
        // Test interleaving reads and seeks
        let data: Vec<u8> = (0..200).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        let mut buf = vec![0; 3];

        reader.read(&mut buf).unwrap();
        reader.seek(SeekFrom::Start(50)).unwrap();
        reader.read(&mut buf).unwrap();
        reader.seek(SeekFrom::Start(100)).unwrap();
        reader.read(&mut buf).unwrap();
        reader.seek(SeekFrom::Start(150)).unwrap();
        reader.read(&mut buf).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read(&mut buf).unwrap();

        assert_eq!(buf, &[0, 1, 2]);
    }

    #[test]
    fn deadlock_test_background_thread_waiting_on_seek() {
        // Test scenario where background thread is waiting for buffer during seek
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(5, 1, cursor);

        // Read to consume all buffers
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();

        // Seek - this should work even with queue length 1
        reader.seek(SeekFrom::Start(50)).unwrap();

        // Read to verify seek worked
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[50, 51, 52, 53, 54]);
    }

    #[test]
    fn deadlock_test_seek_without_buffer() {
        // Test seek when we don't have a current buffer
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek immediately without reading first (no buffer held)
        reader.seek(SeekFrom::Start(50)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[50, 51, 52, 53, 54]);
    }

    #[test]
    fn deadlock_test_drop_while_processing() {
        // Test dropping SmartBuf while background thread is processing
        // This should not deadlock - background thread should detect Stop command
        let data: Vec<u8> = (0..255).cycle().take(1000).collect();
        let cursor = Cursor::new(data);

        // Create and immediately drop
        {
            let reader = SmartBuf::with_capacity(10, 2, cursor);
            drop(reader);
        }
        // If we get here, no deadlock occurred
    }

    #[test]
    fn deadlock_test_drop_during_read() {
        // Test dropping while reading is in progress
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data);
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Start reading
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();

        // Drop immediately after read
        drop(reader);
        // If we get here, no deadlock occurred
    }

    #[test]
    fn deadlock_test_drop_during_seek() {
        // Test dropping while seek is in progress
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data);
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Start seek
        reader.seek(SeekFrom::Start(50)).unwrap();

        // Drop immediately after seek
        drop(reader);
        // If we get here, no deadlock occurred
    }

    #[test]
    fn deadlock_test_stress_rapid_operations() {
        // Stress test with rapid operations
        let data: Vec<u8> = (0..255).cycle().take(500).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Mix of reads and seeks
        for i in 0..50 {
            if i % 3 == 0 {
                reader.seek(SeekFrom::Start((i * 10) as u64)).unwrap();
            }
            let mut buf = vec![0; 3];
            let n = reader.read(&mut buf).unwrap();
            assert!(n > 0);
        }
    }

    #[test]
    fn deadlock_test_seek_to_same_position_multiple_times() {
        // Test seeking to the same position multiple times
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to same position multiple times
        for _ in 0..10 {
            reader.seek(SeekFrom::Start(50)).unwrap();
        }

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[50, 51, 52, 53, 54]);
    }

    #[test]
    fn deadlock_test_seek_forward_then_backward_rapidly() {
        // Test alternating forward and backward seeks
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Alternate forward and backward
        reader.seek(SeekFrom::Start(50)).unwrap();
        reader.seek(SeekFrom::Start(10)).unwrap();
        reader.seek(SeekFrom::Start(80)).unwrap();
        reader.seek(SeekFrom::Start(20)).unwrap();
        reader.seek(SeekFrom::Start(90)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[90, 91, 92, 93, 94]);
    }

    #[test]
    fn deadlock_test_read_large_spanning_multiple_buffers_after_seek() {
        // Test reading large amount that spans buffers after a seek
        let data: Vec<u8> = (0..200).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to middle
        reader.seek(SeekFrom::Start(100)).unwrap();

        // Read large amount that spans multiple buffers
        let mut buf = vec![0; 50];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 50);
        assert_eq!(&buf, &data[100..150]);
    }

    #[test]
    fn deadlock_test_seek_while_buffer_is_full() {
        // Test seeking when we have a full buffer
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read to fill buffer
        let mut buf = vec![0; 10];
        reader.read(&mut buf).unwrap();

        // Seek while we have a full buffer
        reader.seek(SeekFrom::Start(50)).unwrap();

        // Read again
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[50, 51, 52, 53, 54, 55, 56, 57, 58, 59]);
    }

    #[test]
    fn deadlock_test_seek_while_buffer_is_partially_read() {
        // Test seeking when we have a partially read buffer
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read part of buffer
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[0, 1, 2, 3, 4]);

        // Seek while we have a partially read buffer
        reader.seek(SeekFrom::Start(50)).unwrap();

        // Read again
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[50, 51, 52, 53, 54]);
    }

    #[test]
    fn deadlock_test_multiple_seek_current_operations() {
        // Test multiple SeekFrom::Current operations in sequence
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Chain SeekFrom::Current operations
        reader.seek(SeekFrom::Current(10)).unwrap();
        reader.seek(SeekFrom::Current(10)).unwrap();
        reader.seek(SeekFrom::Current(-5)).unwrap();
        reader.seek(SeekFrom::Current(15)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[30, 31, 32, 33, 34]);
    }

    #[test]
    fn deadlock_test_seek_end_then_seek_start() {
        // Test seeking to end then to start
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        reader.seek(SeekFrom::End(0)).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[0, 1, 2, 3, 4]);
    }

    #[test]
    fn deadlock_test_seek_current_negative_beyond_buffer() {
        // Test SeekFrom::Current with large negative value
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read forward
        reader.seek(SeekFrom::Start(50)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();

        // Seek backward beyond current buffer
        reader.seek(SeekFrom::Current(-40)).unwrap();

        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[15, 16, 17, 18, 19]);
    }

    #[test]
    fn deadlock_test_minimal_buffer_size() {
        // Test with minimal buffer size (1 byte)
        let data: Vec<u8> = (0..50).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(1, 1, cursor);

        // Read and seek operations
        for i in 0..10 {
            reader.seek(SeekFrom::Start(i)).unwrap();
            let mut buf = vec![0; 1];
            let n = reader.read(&mut buf).unwrap();
            assert_eq!(n, 1);
            assert_eq!(buf[0], i as u8);
        }
    }

    #[test]
    fn deadlock_test_all_buffers_in_use() {
        // Test scenario where all buffers are in use
        let data: Vec<u8> = (0..200).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read through multiple buffers to ensure all are in use
        for _ in 0..10 {
            let mut buf = vec![0; 10];
            reader.read(&mut buf).unwrap();
        }

        // Now seek - should still work
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[0, 1, 2, 3, 4]);
    }

    #[test]
    fn deadlock_test_rapid_seek_current_forward_backward() {
        // Test rapid SeekFrom::Current with forward and backward
        let data: Vec<u8> = (0..200).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Rapid forward and backward seeks
        reader.seek(SeekFrom::Current(20)).unwrap();
        reader.seek(SeekFrom::Current(20)).unwrap();
        reader.seek(SeekFrom::Current(-10)).unwrap();
        reader.seek(SeekFrom::Current(20)).unwrap();
        reader.seek(SeekFrom::Current(-15)).unwrap();

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[35, 36, 37, 38, 39]);
    }

    #[test]
    fn deadlock_test_seek_after_eof() {
        // Test seeking after reaching EOF
        let data: Vec<u8> = (0..50).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read to EOF
        let mut buf = vec![0; 100];
        reader.read(&mut buf).unwrap();

        // Seek after EOF
        reader.seek(SeekFrom::Start(10)).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..40], &data[10..50]);
    }

    #[test]
    fn edge_case_empty_file() {
        // Test with empty file
        let data: Vec<u8> = vec![];
        let cursor = Cursor::new(data);
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read should return 0
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);

        // Seek to start should work
        reader.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(reader.position(), 0);

        // Read again should return 0
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn edge_case_buffer_larger_than_file() {
        // Test with buffer size larger than file size
        let data: Vec<u8> = (0..10).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(100, 2, cursor);

        // Should read entire file in one buffer
        let mut buf = vec![0; 100];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf[..n], &data[..]);
    }

    #[test]
    fn edge_case_seek_to_file_end() {
        // Test seeking to exactly the end of file
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to end
        reader.seek(SeekFrom::Start(20)).unwrap();
        assert_eq!(reader.position(), 20);

        // Read should return 0
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn edge_case_seek_beyond_file_end() {
        // Test seeking beyond file end
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek beyond end
        reader.seek(SeekFrom::Start(50)).unwrap();
        assert_eq!(reader.position(), 50);

        // Read should return 0 (EOF)
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn edge_case_read_exactly_buffer_size() {
        // Test reading exactly buffer size
        let data: Vec<u8> = (0..30).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read exactly one buffer
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf, &data[..10]);

        // Next read should get next buffer
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf, &data[10..20]);
    }

    #[test]
    fn edge_case_read_multiple_of_buffer_size() {
        // Test reading multiple times buffer size
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read exactly 5 buffers worth
        let mut buf = vec![0; 50];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 50);
        assert_eq!(&buf, &data[..50]);
    }

    #[test]
    fn edge_case_seek_current_zero() {
        // Test SeekFrom::Current(0) - should be no-op
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read some
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 5);

        // Seek current by 0
        reader.seek(SeekFrom::Current(0)).unwrap();
        assert_eq!(reader.position(), 5);

        // Should continue reading from same position
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[5, 6, 7, 8, 9]);
    }

    #[test]
    fn edge_case_seek_current_large_forward() {
        // Test SeekFrom::Current with very large forward value
        let data: Vec<u8> = (0..200).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        reader.seek(SeekFrom::Current(100)).unwrap();
        assert_eq!(reader.position(), 100);

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[100, 101, 102, 103, 104]);
    }

    #[test]
    fn edge_case_seek_current_large_backward() {
        // Test SeekFrom::Current with very large backward value
        let data: Vec<u8> = (0..200).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Move forward first
        reader.seek(SeekFrom::Start(100)).unwrap();
        assert_eq!(reader.position(), 100);

        // Seek backward
        reader.seek(SeekFrom::Current(-80)).unwrap();
        assert_eq!(reader.position(), 20);

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[20, 21, 22, 23, 24]);
    }

    #[test]
    fn edge_case_seek_from_end_positive() {
        // Test SeekFrom::End with positive offset
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek from end with positive offset (beyond end)
        let pos = reader.seek(SeekFrom::End(10)).unwrap();
        // The position should be file_size + offset = 20 + 10 = 30
        // The seek operation returns the actual position
        assert_eq!(pos, 30);

        // Position should match what was returned
        assert_eq!(reader.position(), pos);

        // Read should return 0 (EOF)
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn edge_case_seek_from_end_at_start() {
        // Test SeekFrom::End(-file_size) should go to start
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        reader.seek(SeekFrom::End(-20)).unwrap();
        assert_eq!(reader.position(), 0);

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[0, 1, 2, 3, 4]);
    }

    #[test]
    fn edge_case_seek_from_end_beyond_start() {
        // Test SeekFrom::End with offset larger than file size
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // This should cause an error due to overflow
        assert!(reader.seek(SeekFrom::End(-100)).is_err());
    }

    #[test]
    fn edge_case_position_tracking_through_multiple_operations() {
        // Test position tracking accuracy through complex operations
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        assert_eq!(reader.position(), 0);

        // Read
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 5);

        // Seek
        reader.seek(SeekFrom::Start(20)).unwrap();
        assert_eq!(reader.position(), 20);

        // Read
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 25);

        // Seek current
        reader.seek(SeekFrom::Current(-10)).unwrap();
        assert_eq!(reader.position(), 15);

        // Read
        reader.read(&mut buf).unwrap();
        assert_eq!(reader.position(), 20);
    }

    #[test]
    fn edge_case_read_zero_bytes() {
        // Test reading zero bytes
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read zero bytes should return 0 immediately
        let mut buf = vec![0; 0];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);

        // Position should not change
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn edge_case_read_exact_insufficient_data() {
        // Test read_exact when there's insufficient data
        let data: Vec<u8> = (0..10).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(5, 2, cursor);

        // Try to read more than available
        let mut buf = vec![0; 20];
        let result = reader.read_exact(&mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn edge_case_read_exact_exact_amount() {
        // Test read_exact with exact amount available
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read exact amount
        let mut buf = vec![0; 20];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[..]);
    }

    #[test]
    fn edge_case_seek_to_same_position_multiple_times() {
        // Test seeking to same position repeatedly
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to same position multiple times
        for _ in 0..5 {
            reader.seek(SeekFrom::Start(10)).unwrap();
            assert_eq!(reader.position(), 10);
        }

        // Should still read correctly
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[10, 11, 12, 13, 14]);
    }

    #[test]
    fn edge_case_seek_within_buffer_multiple_times() {
        // Test seeking within buffer multiple times
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(20, 2, cursor);

        // Read to fill buffer
        let mut buf = vec![0; 10];
        reader.read(&mut buf).unwrap();

        // Seek within buffer multiple times
        reader.seek(SeekFrom::Start(5)).unwrap();
        reader.seek(SeekFrom::Start(3)).unwrap();
        reader.seek(SeekFrom::Start(7)).unwrap();
        reader.seek(SeekFrom::Start(2)).unwrap();

        // Should read from last seek position
        reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..5], &[2, 3, 4, 5, 6]);
    }

    #[test]
    fn edge_case_read_to_end_multiple_times() {
        // Test read_to_end called multiple times
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // First read_to_end
        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result, data.clone());

        // Second read_to_end should return empty
        let mut result2 = Vec::new();
        reader.read_to_end(&mut result2).unwrap();
        assert_eq!(result2, vec![]);
    }

    #[test]
    fn edge_case_seek_after_read_to_end() {
        // Test seeking after read_to_end
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read to end
        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();

        // Seek back
        reader.seek(SeekFrom::Start(5)).unwrap();

        // Should be able to read again
        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[5, 6, 7, 8, 9]);
    }

    #[test]
    fn edge_case_large_queue_length() {
        // Test with very large queue length
        let data: Vec<u8> = (0..100).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 20, cursor);

        // Should work normally
        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn edge_case_very_small_buffer_size() {
        // Test with very small buffer size (1 byte)
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(1, 2, cursor);

        // Read should work
        let mut buf = vec![0; 20];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 20);
        assert_eq!(&buf[..n], &data[..]);
    }

    #[test]
    fn edge_case_read_single_byte_multiple_times() {
        // Test reading one byte at a time
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Read one byte at a time
        for i in 0..20 {
            let mut buf = vec![0; 1];
            let n = reader.read(&mut buf).unwrap();
            assert_eq!(n, 1);
            assert_eq!(buf[0], i);
            assert_eq!(reader.position(), (i + 1) as u64);
        }
    }

    #[test]
    fn edge_case_seek_then_read_partial() {
        // Test seeking then reading only part of buffer
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek to middle
        reader.seek(SeekFrom::Start(10)).unwrap();

        // Read only part of buffer
        let mut buf = vec![0; 3];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[10, 11, 12]);

        // Seek again and read more
        reader.seek(SeekFrom::Start(15)).unwrap();
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[15, 16, 17]);
    }

    #[test]
    fn edge_case_seek_current_at_start() {
        // Test SeekFrom::Current at start of file
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Seek from current at start
        reader.seek(SeekFrom::Current(5)).unwrap();
        assert_eq!(reader.position(), 5);

        let mut buf = vec![0; 5];
        reader.read(&mut buf).unwrap();
        assert_eq!(buf, &[5, 6, 7, 8, 9]);
    }

    #[test]
    fn edge_case_seek_current_negative_at_start() {
        // Test SeekFrom::Current with negative at start (should error)
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // Should error due to overflow
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
    }

    #[test]
    fn edge_case_buffer_exactly_one_byte() {
        // Test with buffer size of 1 and queue length of 1
        let data: Vec<u8> = (0..10).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(1, 1, cursor);

        // Should still work
        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn edge_case_seek_from_end_zero() {
        // Test SeekFrom::End(0) - should go to end
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        // SeekFrom::End(0) should position at end of file
        let pos = reader.seek(SeekFrom::End(0)).unwrap();
        // The position should be file_size = 20
        assert_eq!(pos, 20);
        assert_eq!(reader.position(), 20);

        // Read should return 0
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn edge_case_read_after_seek_to_exact_end() {
        // Test reading after seeking to exact end
        let data: Vec<u8> = (0..20).collect();
        let cursor = Cursor::new(data.clone());
        let mut reader = SmartBuf::with_capacity(10, 2, cursor);

        reader.seek(SeekFrom::Start(20)).unwrap();
        assert_eq!(reader.position(), 20);

        // Read should return 0
        let mut buf = vec![0; 10];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);

        // Seek back and read should work
        reader.seek(SeekFrom::Start(10)).unwrap();
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf, &data[10..20]);
    }
}
