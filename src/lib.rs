use std::io::{self, Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::thread::{self, JoinHandle};
use crossbeam_channel::{unbounded as channel, Receiver, Sender};

const DEFAULT_BUF_SIZE: usize = 8 * 1024;
const DEFAULT_QUEUE_LEN: usize = 2;

#[derive(Debug)]
struct Buffer {
    data: Box<[u8]>,
    start_pos: u64,  // Absolute position where this buffer starts
    end: usize,      // Valid data end in buffer
}

impl Buffer {
    fn new(size: usize, start_pos: u64) -> Buffer {
        assert!(size > 0);
        Buffer {
            data: vec![0; size].into_boxed_slice(),
            start_pos,
            end: 0,
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
    Seek(SeekFrom),
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
pub struct SmartBuf<R: Read + Seek + Send> {
    // Channels for communication with background thread
    cmd_send: Option<Sender<Command>>,
    buf_recv: Receiver<io::Result<Buffer>>,
    buf_send: Sender<Option<Buffer>>,
    
    // Current buffer state
    buffer: Option<Buffer>,
    buf_pos: usize,  // Position within current buffer
    absolute_pos: u64,  // Absolute position in stream
    
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

        let mut background_reader = BackgroundReader::new(
            empty_recv,
            full_send,
            cmd_recv,
            bufsize,
        );

        let handle = thread::spawn(move || {
            background_reader.serve(reader);
        });

        SmartBuf {
            cmd_send: Some(cmd_send),
            buf_recv: full_recv,
            buf_send: empty_send,
            buffer: None,
            buf_pos: 0,
            absolute_pos: 0,
            bufsize,
            handle: Some(handle),
            _phantom: PhantomData,
        }
    }

    /// Returns the current absolute position in the stream.
    pub fn position(&self) -> u64 {
        self.absolute_pos
    }

    /// Returns the buffer size.
    pub fn buffer_size(&self) -> usize {
        self.bufsize
    }

    fn get_next_buffer(&mut self) -> io::Result<()> {
        // Return current buffer to pool if it exists
        if let Some(buf) = self.buffer.take() {
            self.buf_send.send(Some(buf)).ok();
        }

        // Get next buffer
        match self.buf_recv.recv() {
            Ok(Ok(buffer)) => {
                self.buffer = Some(buffer);
                self.buf_pos = 0;
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "background thread terminated",
            )),
        }
    }
}

impl<R: Read + Seek + Send + 'static> Read for SmartBuf<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut total_read = 0;

        while total_read < buf.len() {
            // Ensure we have a buffer
            if self.buffer.is_none() {
                self.get_next_buffer()?;
            }

            let current = self.buffer.as_ref().unwrap();
            
            // Check if we're at the end of current buffer
            if self.buf_pos >= current.end {
                // If buffer is empty (EOF), we're done
                if current.end == 0 {
                    break;
                }
                // Buffer exhausted, get next one
                self.get_next_buffer()?;
                continue;
            }

            // Read from current buffer
            let available = current.end - self.buf_pos;
            let to_read = std::cmp::min(available, buf.len() - total_read);
            let start = self.buf_pos;
            let end = start + to_read;

            buf[total_read..total_read + to_read]
                .copy_from_slice(&current.data[start..end]);

            self.buf_pos += to_read;
            self.absolute_pos += to_read as u64;
            total_read += to_read;

            // If we've consumed the entire buffer, get the next one
            if self.buf_pos >= current.end {
                self.get_next_buffer()?;
            }
        }

        Ok(total_read)
    }
}

impl<R: Read + Seek + Send + 'static> Seek for SmartBuf<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target_pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(n) => {
                if n >= 0 {
                    self.absolute_pos.checked_add(n as u64)
                } else {
                    self.absolute_pos.checked_sub((-n) as u64)
                }
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "seek overflow")
                })?
            }
            SeekFrom::End(_n) => {
                // For SeekFrom::End, we need to know the file size
                // Send the command to background thread to handle it
                if let Some(sender) = &self.cmd_send {
                    // Drain any already-filled buffers from the channel - these are stale
                    while self.buf_recv.try_recv().is_ok() {}
                    
                    sender.send(Command::Seek(pos)).ok();
                    // Invalidate current buffer and provide empty buffer if needed
                    if let Some(buf) = self.buffer.take() {
                        self.buf_send.send(Some(buf)).ok();
                    } else {
                        // Provide an empty buffer so background thread can start filling
                        // We don't know the exact position yet, but the background thread
                        // will update it after seeking
                        let empty_buffer = Buffer::new(self.bufsize, 0);
                        self.buf_send.send(Some(empty_buffer)).ok();
                    }
                    self.buf_pos = 0;
                    // Get new buffer to update absolute_pos
                    self.get_next_buffer()?;
                    return Ok(self.absolute_pos);
                }
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "background thread terminated",
                ));
            }
        };

        // Check if target is within current buffer
        if let Some(ref buffer) = self.buffer {
            if target_pos >= buffer.start_pos {
                let offset = (target_pos - buffer.start_pos) as usize;
                if offset <= buffer.end {
                    // Seek is within current buffer
                    self.buf_pos = offset;
                    self.absolute_pos = target_pos;
                    return Ok(target_pos);
                }
            }
        }

        // Seek is outside current buffer - send command to background thread
        if let Some(sender) = &self.cmd_send {
            // Drain any already-filled buffers from the channel - these are stale
            // and were filled before the seek
            while self.buf_recv.try_recv().is_ok() {}
            
            sender.send(Command::Seek(SeekFrom::Start(target_pos))).ok();
            // Invalidate current buffer
            if let Some(buf) = self.buffer.take() {
                // Return the old buffer to the pool
                self.buf_send.send(Some(buf)).ok();
            } else {
                // If we don't have a buffer, we need to provide an empty one
                // so the background thread can start filling from the new position
                let empty_buffer = Buffer::new(self.bufsize, target_pos);
                self.buf_send.send(Some(empty_buffer)).ok();
            }
            self.buf_pos = 0;
            self.absolute_pos = target_pos;
            // Get new buffer from background thread
            // Note: This will block until background thread processes the seek
            self.get_next_buffer()?;
            Ok(target_pos)
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "background thread terminated",
            ))
        }
    }
}

impl<R: Read + Seek + Send> Drop for SmartBuf<R> {
    fn drop(&mut self) {
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
        }
    }

    fn serve<R: Read + Seek>(&mut self, mut reader: R) {
        let mut current_pos = 0u64;

        loop {
            // Check for commands - use blocking recv with timeout-like behavior
            // We need to handle seeks promptly, but also process buffers
            let had_command = self.cmd_recv.try_recv().ok().map(|cmd| {
                match cmd {
                    Command::Seek(pos) => {
                        // Seek the underlying reader
                        match reader.seek(pos) {
                            Ok(new_pos) => {
                                current_pos = new_pos;
                                // Don't drain buffers - the next buffer we receive
                                // will be filled from the new position
                            }
                            Err(e) => {
                                self.full_send.send(Err(e)).ok();
                                return false; // Signal to break
                            }
                        }
                        true // Continue
                    }
                    Command::Stop => false, // Signal to break
                }
            });

            // If we got a Stop command, break
            if let Some(false) = had_command {
                break;
            }

            // Try to get an empty buffer (blocking if we just processed a seek)
            // Use recv if we just processed a seek, to ensure we get the buffer
            // that was sent after the seek
            let buffer_result = if had_command.is_some() {
                // After a seek, wait for the buffer that was sent
                self.empty_recv.recv()
            } else {
                // Normal operation, use non-blocking
                match self.empty_recv.try_recv() {
                    Ok(b) => Ok(b),
                    Err(_) => {
                        // No buffer and no command, wait a bit
                        thread::yield_now();
                        continue;
                    }
                }
            };

            match buffer_result {
                Ok(Some(mut buffer)) => {
                    // Update buffer start position to current position
                    buffer.start_pos = current_pos;
                    match buffer.refill(&mut reader) {
                        Ok(()) => {
                            current_pos += buffer.end as u64;
                            self.full_send.send(Ok(buffer)).ok();
                        }
                        Err(e) => {
                            let kind = e.kind();
                            let should_break = kind != io::ErrorKind::Interrupted;
                            self.full_send.send(Err(e)).ok();
                            if should_break {
                                break;
                            }
                        }
                    }
                }
                Ok(None) => {
                    // Shutdown signal
                    break;
                }
                Err(_) => {
                    // Channel closed
                    break;
                }
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
}

