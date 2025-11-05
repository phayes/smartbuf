# smartbuf

[![Crates.io](https://img.shields.io/crates/v/smartbuf.svg)](https://crates.io/crates/smartbuf)
[![docs.rs](https://docs.rs/smartbuf/badge.svg)](https://docs.rs/smartbuf)

A high-performance buffered reader with background thread pre-fetching and full seek support.

`SmartBuf` wraps any `Read + Seek` implementation and provides:
- **Off-thread pre-fetch buffering** for improved read performance
- **Full seek support** with optimization for seeks within buffered data
- **Configurable buffer sizes** and queue lengths for fine-tuning performance

## Use Cases

`SmartBuf` is particularly well-suited for wrapping readers that perform **network requests** or other **slow or inconsistent I/O operations**. The background thread pre-fetching helps mask latency by reading data ahead of time, while the buffering system handles variability in network speed and reliability.

Common scenarios include:
- **HTTP/HTTPS responses**: Wrapping network stream readers from libraries like `reqwest`, `hyper`, or `ureq`
- **Remote file access**: Reading from cloud storage APIs or remote file systems
- **Database BLOB streams**: Reading large binary objects from databases over network connections
- **Any slow or variable-latency I/O**: Situations where read operations may block or take inconsistent amounts of time

For fast local I/O (like reading from memory or fast SSDs), the overhead of background threading may outweigh the benefits, and standard buffered readers may be more appropriate.

## Features

- **Background thread pre-fetching**: Data is read ahead of time in a background thread, reducing blocking on I/O operations
- **Intelligent seek optimization**: Seeks within the current buffer are handled instantly without touching the underlying reader
- **Configurable performance**: Adjust buffer size and queue length based on your use case
- **Standard trait implementation**: Implements `std::io::Read` and `std::io::Seek` for drop-in compatibility

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
smartbuf = "0.1.0"
```

## Quick Example

```rust
use smartbuf::SmartBuf;
use std::io::{Read, Seek, SeekFrom, Cursor};

let data = b"Hello, world! This is a test.";
let cursor = Cursor::new(data);
let mut reader = SmartBuf::new(cursor);

// Read some data
let mut buf = vec![0; 5];
reader.read(&mut buf).unwrap();
assert_eq!(&buf, b"Hello");

// Seek back to the beginning
reader.seek(SeekFrom::Start(0)).unwrap();

// Read again
let mut buf = vec![0; 5];
reader.read(&mut buf).unwrap();
assert_eq!(&buf, b"Hello");

// Seek forward
reader.seek(SeekFrom::Current(7)).unwrap();
let mut buf = vec![0; 4];
reader.read(&mut buf).unwrap();
assert_eq!(&buf, b"orld");
```

## Usage

### Basic Usage

```rust
use smartbuf::SmartBuf;
use std::io::{Read, Seek, SeekFrom};
use std::fs::File;

let file = File::open("data.bin")?;
let mut reader = SmartBuf::new(file);

// Read data
let mut buffer = vec![0; 1024];
let bytes_read = reader.read(&mut buffer)?;

// Seek to a specific position
reader.seek(SeekFrom::Start(1000))?;

// Continue reading from the new position
reader.read(&mut buffer)?;
```

### Custom Buffer Configuration

For fine-tuned performance, you can specify the buffer size and queue length:

```rust
use smartbuf::SmartBuf;
use std::io::Cursor;

let data = vec![0u8; 1024 * 1024]; // 1MB of data
let cursor = Cursor::new(data);

// Create with custom buffer size (16KB) and queue length (4)
let mut reader = SmartBuf::with_capacity(16 * 1024, 4, cursor);

// Larger buffers and more queue slots can improve throughput
// at the cost of increased memory usage
```

### Seeking Operations

`SmartBuf` supports all standard seek operations:

```rust
use smartbuf::SmartBuf;
use std::io::{Read, Seek, SeekFrom, Cursor};

let data: Vec<u8> = (0..100).collect();
let cursor = Cursor::new(data);
let mut reader = SmartBuf::with_capacity(10, 2, cursor);

// Seek to absolute position
reader.seek(SeekFrom::Start(50)).unwrap();

// Seek relative to current position (forward)
reader.seek(SeekFrom::Current(10)).unwrap();

// Seek relative to current position (backward)
reader.seek(SeekFrom::Current(-5)).unwrap();

// Seek from end of file
reader.seek(SeekFrom::End(-10)).unwrap();
```

### Reading Entire Files

```rust
use smartbuf::SmartBuf;
use std::io::Read;

let cursor = Cursor::new(vec![1, 2, 3, 4, 5]);
let mut reader = SmartBuf::new(cursor);

let mut contents = Vec::new();
reader.read_to_end(&mut contents).unwrap();
```

## Examples

### Reading with Seeking

```rust
use smartbuf::SmartBuf;
use std::io::{Read, Seek, SeekFrom, Cursor};

let data: Vec<u8> = (0..1000).collect();
let cursor = Cursor::new(data.clone());
let mut reader = SmartBuf::with_capacity(100, 2, cursor);

// Read first 50 bytes
let mut buf = vec![0; 50];
reader.read(&mut buf).unwrap();
assert_eq!(&buf, &data[0..50]);

// Seek to middle
reader.seek(SeekFrom::Start(500)).unwrap();
let mut buf = vec![0; 50];
reader.read(&mut buf).unwrap();
assert_eq!(&buf, &data[500..550]);

// Seek back
reader.seek(SeekFrom::Start(0)).unwrap();
let mut buf = vec![0; 50];
reader.read(&mut buf).unwrap();
assert_eq!(&buf, &data[0..50]);
```

### Large File Processing

```rust
use smartbuf::SmartBuf;
use std::io::{Read, Seek, SeekFrom};
use std::fs::File;

let file = File::open("large_file.bin")?;
let mut reader = SmartBuf::with_capacity(64 * 1024, 4, file);

// Process file in chunks
let mut buffer = vec![0; 1024 * 1024]; // 1MB chunks
loop {
    match reader.read(&mut buffer)? {
        0 => break, // EOF
        n => {
            // Process buffer[..n]
            process_chunk(&buffer[..n]);
        }
    }
}
```

## Requirements

- Rust 1.38.0 or later
- Dependencies: `crossbeam-channel` and `crossbeam-utils` for thread-safe communication

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
