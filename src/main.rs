use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::{env, io};

#[derive(Debug)]
struct WaylandFrame {
    id: u32,
    opcode: u16,
    arguments: Vec<u8>,
}

impl WaylandFrame {
    fn new(id: u32, opcode: u16, arguments: Vec<u8>) -> WaylandFrame {
        WaylandFrame {
            id,
            opcode,
            arguments,
        }
    }

    fn serialize(&self) -> Vec<u8> {
        let mut buffer = Vec::new();
        let total_size: u16 = 8 + self.arguments.len() as u16;
        buffer.extend_from_slice(&self.id.to_le_bytes());
        buffer.extend_from_slice(&self.opcode.to_le_bytes());
        buffer.extend_from_slice(&total_size.to_le_bytes());
        buffer.extend_from_slice(&self.arguments);
        buffer
    }

    fn try_parse(buffer: &mut Vec<u8>) -> std::io::Result<Option<Self>> {
        if buffer.len() < 8 {
            return Ok(None);
        }
        let id = u32::from_ne_bytes(buffer[0..4].try_into().unwrap());
        let opcode = u16::from_ne_bytes(buffer[4..6].try_into().unwrap());
        let total_size = u16::from_ne_bytes(buffer[6..8].try_into().unwrap());

        if total_size < 8 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Za mały rozmiar ramki",
            ));
        }
        if buffer.len() < total_size as usize {
            return Ok(None);
        }
        let mut raw_frame: Vec<u8> = buffer.drain(0..total_size as usize).collect();
        let payload = raw_frame.split_off(8);
        Ok(Some(Self {
            id,
            opcode,
            arguments: payload,
        }))
    }
}

#[derive(Debug)]
struct FrameDecoder {
    buffer: Vec<u8>,
    cursor: usize,
}

impl FrameDecoder {
    fn new(payload: Vec<u8>) -> FrameDecoder {
        FrameDecoder {
            buffer: payload,
            cursor: 0,
        }
    }
    fn read_uint(&mut self) -> u32 {
        let bytes = &self.buffer[self.cursor..self.cursor + 4];
        self.cursor += 4;
        u32::from_ne_bytes(bytes.try_into().unwrap())
    }
    fn read_string(&mut self) -> String {
        let length = self.read_uint();
        let string =
            String::from_utf8(self.buffer[self.cursor..self.cursor + length as usize - 1].to_vec())
                .unwrap();

        self.cursor += length as usize;

        let reminder = length % 4;
        if reminder != 0 {
            self.cursor += (4 - reminder) as usize;
        }
        string
    }
}

struct FrameEncoder {
    buffer: Vec<u8>,
}

impl FrameEncoder {
    fn new() -> FrameEncoder {
        FrameEncoder { buffer: Vec::new() }
    }
    fn padding(&mut self) {
        let length = self.buffer.len();
        let reminder = length % 4;
        if reminder != 0 {
            let offset = 4 - reminder;
            self.buffer.resize(length + offset, 0);
        }
    }
    fn write_uint(&mut self, x: u32) {
        self.buffer.extend_from_slice(&x.to_ne_bytes());
    }
    fn write_int(&mut self, x: i32) {
        self.buffer.extend_from_slice(&x.to_ne_bytes());
    }
    fn write_string(&mut self, text: &str) {
        let length: u32 = text.len() as u32 + 1;
        self.write_uint(length);
        self.buffer.extend_from_slice(text.as_bytes());
        self.buffer.push(0x00);
        self.padding()
    }
    fn get_buffer(self) -> Vec<u8> {
        self.buffer
    }
}

enum WaylandEvents {}

#[derive(Debug)]
enum StreamError {
    ChannelClosed(mpsc::SendError<WaylandFrame>),

    Io(std::io::Error),
    Disconnected,
}

impl From<mpsc::SendError<WaylandFrame>> for StreamError {
    fn from(err: mpsc::SendError<WaylandFrame>) -> Self {
        StreamError::ChannelClosed(err)
    }
}

impl From<std::io::Error> for StreamError {
    fn from(err: std::io::Error) -> Self {
        StreamError::Io(err)
    }
}

struct WaylandStream {
    stream: UnixStream,
    stream_buffer: Vec<u8>,
}

impl WaylandStream {
    fn init() -> Result<WaylandStream, &'static str> {
        let env_name = "XDG_RUNTIME_DIR";
        let wayland_dir = match env::var(env_name) {
            Ok(val) => val,
            Err(_) => "".to_string(),
        };
        if wayland_dir.is_empty() {
            panic!("Brak możliwości znalezienia XDG_RUNTIME_DIR");
        }
        let wayland_display = match env::var("WAYLAND_DISPLAY") {
            Ok(val) => val,
            Err(_) => "wayland-0".to_string(),
        };
        let mut path = PathBuf::from(wayland_dir);
        path.push(wayland_display);
        let stream = UnixStream::connect(path).map_err(|_| "nieudało się połączyć z gniazdem")?;
        Ok(WaylandStream {
            stream: stream,
            stream_buffer: Vec::new(),
        })
    }

    fn read(&mut self, event: &mpsc::Sender<WaylandFrame>) -> Result<(), StreamError> {
        if let Some(frame) = WaylandFrame::try_parse(&mut self.stream_buffer)? {
            event.send(frame)?;
            return Ok(());
        }

        let mut tmp_buffer = [0u8; 4096];
        match self.stream.read(&mut tmp_buffer) {
            Ok(0) => Err(StreamError::Disconnected),
            Ok(bytes) => {
                self.stream_buffer.extend_from_slice(&tmp_buffer[..bytes]);
                if let Some(frame) = WaylandFrame::try_parse(&mut self.stream_buffer)? {
                    event.send(frame)?;
                }
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(StreamError::Io(e)),
        }
    }

    fn write(&mut self, request: &mpsc::Receiver<WaylandFrame>) -> Result<(), StreamError> {
        let mut end = false;
        loop {
            match request.try_recv() {
                Ok(req) => {
                    self.stream.write_all(&req.serialize())?;
                    end = true;
                    println!("Packet recived");
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Err(StreamError::Disconnected),
            }
        }
        if end {
            self.stream.flush()?;
            println!("Flushed");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WaylandObject {
    // Global objects
    WlRegistry,
    WlDisplay,
    WlCompositor,
    XdgWmBase,
    WlShm,

    // local objects binded to window
    WlSurface { window_id: u32 },
    XdgSurface { window_id: u32 },
    XdgTopLevel { window_id: u32 },
    WlShmPool { window_id: u32 },
    WlBuffer { window_id: u32 },
}

struct ObjectManager {
    next_id: u32,
    free_id: Vec<u32>,
    objects: HashMap<u32, WaylandObject>,
}

impl ObjectManager {
    fn init() -> Self {
        ObjectManager {
            next_id: 2,
            free_id: Vec::new(),
            objects: HashMap::from([(1, WaylandObject::WlDisplay)]),
        }
    }
    fn set_id_client(&mut self, obj: WaylandObject) -> u32 {
        let id = self.free_id.pop().unwrap_or_else(|| {
            let current_id = self.next_id;
            self.next_id += 1;
            current_id
        });
        self.objects.insert(id, obj);
        id
    }

    fn set_id_serv(&mut self, obj: WaylandObject, id: u32) {
        let _ = self.objects.insert(id, obj);
    }

    fn from_frame(&self, frame: WaylandFrame) -> Option<WaylandObject> {
        let id = frame.id;
        let obj = self.objects.get(&id);
        match obj {
            Some(x) => return Some(*x),
            None => return None,
        }
    }

    fn get_id(&mut self, obj: &WaylandObject) -> Option<u32> {
        for (key, val) in self.objects.iter() {
            if obj == val {
                return Some(*key);
            }
        }
        None
    }

    fn pop(&mut self, frame: WaylandFrame) {
        let id = frame.id;
        self.objects.remove_entry(&id);
        self.free_id.push(id);
    }
}

fn main() {
    let stream = WaylandStream::init().unwrap();

    println!("hello World");
}
