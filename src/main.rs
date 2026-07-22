use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

// local Objects ID's
const WL_COMPOSITOR: u32 = 4;
const WL_SHM: u32 = 5;
const XDG_WM_BASE: u32 = 6;

// surface ID's
const WL_SURFACE: u32 = 8;
const XDG_SURFACE: u32 = 10;
const XDG_TOPLEVEL: u32 = 12;
const SHM_POOL: u32 = 13;
const WL_BUFFER: u32 = 14;

fn connect_to_socket() -> Result<UnixStream, &'static str> {
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
    Ok(stream)
}

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

    fn read_from_stream(stream: &mut UnixStream) -> std::io::Result<Self> {
        let mut header_buf = vec![0u8; 8];
        stream.read_exact(&mut header_buf)?;

        let id = u32::from_ne_bytes(header_buf[0..4].try_into().unwrap());
        let opcode = u16::from_ne_bytes(header_buf[4..6].try_into().unwrap());
        let total_size = u16::from_ne_bytes(header_buf[6..8].try_into().unwrap());

        if total_size < 8 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Za mały rozmiar ramki",
            ));
        }

        let read_size = (total_size - 8) as usize;
        let mut payload = vec![0u8; read_size];
        if read_size > 0 {
            stream.read_exact(&mut payload)?;
        }
        Ok(Self {
            id,
            opcode,
            arguments: payload,
        })
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

struct WaylandBuffer {
    file: ManuallyDrop<File>,
    ptr: *mut u8,
    size: usize,
}

impl WaylandBuffer {
    fn new(size: usize) -> std::io::Result<Self> {
        let name = std::ffi::CString::new("wayland_shm").unwrap();

        // Przepisane na libc::memfd_create
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size as u64).unwrap();

        // Przepisane na libc::mmap
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            file: ManuallyDrop::new(file),
            ptr: ptr as *mut u8,
            size,
        })
    }
    fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }
    fn get_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Drop for WaylandBuffer {
    fn drop(&mut self) {
        unsafe {
            // Przepisane na libc::munmap
            libc::munmap(self.ptr as *mut libc::c_void, self.size);
            ManuallyDrop::drop(&mut self.file);
        };
    }
}

enum StateMachine {
    SendGetRegistry,
    GetRegistry,
    BindLocalID,
    WaitToBind,
    CreateSurface,
    WaitToXDG,
    CreateXDG,
    WaitToToplevel,
    Configure,
    MainLoop,
}

enum WaylandEvent {
    Configure {
        surface_id: u32,
        width: u32,
        height: u32,
    },
    WindowResize {
        surface_id: u32,
    },
    WindowClose {
        surface_id: u32,
    },
}

struct WaylandConnect {
    stream: UnixStream,
    frame_buffer: Vec<WaylandFrame>,
    buffer: Vec<u8>,
    objects: Vec<u32>,
    next_id: u32,
    wl_compositor: u32,
    wl_shm: u32,
    xdg_wm_base: u32,
    free_id: Vec<u32>,
}

impl WaylandConnect {
    fn init() -> std::io::Result<Self> {
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
        let stream = UnixStream::connect(path)?;
        Ok(WaylandConnect {
            stream,
            buffer: Vec::new(),
            frame_buffer: Vec::new(),
            objects: vec![2],
            next_id: 3,
            wl_compositor: 0,
            wl_shm: 0,
            xdg_wm_base: 0,
            free_id: Vec::new(),
        })
    }

    fn new_id(&mut self) -> u32 {
        let id = self.free_id.pop().unwrap_or_else(|| {
            let current = self.next_id;
            self.next_id = current + 1;
            current
        });
        self.objects.push(id);
        id
    }

    fn sync(&mut self) -> std::io::Result<()> {
        let mut sync = FrameEncoder::new();
        let id = self.new_id();

        sync.write_uint(id);
        let sync_message = WaylandFrame::new(1, 0, sync.get_buffer());
        println!("Sync: {:?} id {}", &sync_message.serialize(), id);
        self.stream.write_all(&sync_message.serialize())?;
        self.stream.flush()?;
        loop {
            if let Some(frame) = self.read_frame()? {
                if frame.id == id && frame.opcode == 0 {
                    println!("{:?}", frame);
                    self.free_id.push(id);
                    self.objects.pop();
                    break;
                }
                self.frame_buffer.push(frame);
            }
        }
        Ok(())
    }

    fn read_frame(&mut self) -> std::io::Result<Option<WaylandFrame>> {
        loop {
            if let Some(frame) = WaylandFrame::try_parse(&mut self.buffer)? {
                return Ok(Some(frame));
            }

            let mut temp_buffer = [0u8; 4096];
            let bytes = self.stream.read(&mut temp_buffer)?;

            if bytes == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&temp_buffer[0..bytes]);
        }
    }

    fn read(&mut self) -> std::io::Result<Option<WaylandFrame>> {
        if !self.frame_buffer.is_empty() {
            return Ok(Some(self.frame_buffer.remove(0)));
        }
        self.read_frame()
    }

    fn bind_global(
        &mut self,
        global_number: u32,
        version: u32,
        name: &str,
        local_id: u32,
    ) -> std::io::Result<()> {
        let mut global_frame = FrameEncoder::new();
        global_frame.write_uint(global_number);
        global_frame.write_string(name);
        global_frame.write_uint(version);
        global_frame.write_uint(local_id);
        let msg_frame = WaylandFrame::new(2, 0, global_frame.get_buffer());
        println!("wl_compositor: {:?}", &msg_frame.serialize());
        self.stream.write_all(&msg_frame.serialize())?;
        Ok(())
    }

    fn bind_registry(&mut self) -> std::io::Result<()> {
        let mut new_request = FrameEncoder::new();
        new_request.write_uint(2);
        let new_message = WaylandFrame::new(1, 1, new_request.get_buffer());
        self.stream.write_all(&new_message.serialize())?;
        self.stream.flush()?;

        loop {
            if let Some(read_message) = self.read()? {
                let mut decoder = FrameDecoder::new(read_message.arguments);

                let number = decoder.read_uint();
                let interface = decoder.read_string();
                let version = decoder.read_uint();

                match interface.as_str() {
                    "wl_compositor" => {
                        self.wl_compositor = self.new_id();
                        self.bind_global(number, version, "wl_compositor", self.wl_compositor)?;
                    }
                    "wl_shm" => {
                        self.wl_shm = self.new_id();
                        self.bind_global(number, version, "wl_shm", self.wl_shm)?;
                    }
                    "xdg_wm_base" => {
                        self.xdg_wm_base = self.new_id();
                        self.bind_global(number, version, "xdg_wm_base", self.xdg_wm_base)?;
                    }
                    _ => {
                        if self.wl_compositor != 0 && self.wl_shm != 0 && self.xdg_wm_base != 0 {
                            break;
                        }
                    }
                }
                println!("name: {number} \n interface: {interface} \n version: {version}");
            }
        }

        self.sync()?;
        Ok(())
    }

    fn wl_surface(&mut self) -> std::io::Result<u32> {
        let id = self.new_id();
        let mut enc_surf = FrameEncoder::new();
        enc_surf.write_uint(id);
        let msg_surf = WaylandFrame::new(WL_COMPOSITOR, 0, enc_surf.get_buffer());
        self.stream.write_all(&msg_surf.serialize())?;
        Ok(id)
    }

    fn xdg_surface(&mut self, surface_id: u32) -> std::io::Result<u32> {
        let id = self.new_id();
        let mut enc_xdg = FrameEncoder::new();
        enc_xdg.write_uint(id); // Nasze nowe ID dla xdg_surface
        enc_xdg.write_uint(surface_id); // Istniejąca wl_surface
        let msg_xdg = WaylandFrame::new(self.xdg_wm_base, 2, enc_xdg.get_buffer());
        self.stream.write_all(&msg_xdg.serialize())?;
        Ok(id)
    }

    fn xdg_toplevel(&mut self, xdg_surface: u32) -> std::io::Result<u32> {
        let id = self.new_id();
        let mut enc_top = FrameEncoder::new();
        enc_top.write_uint(id); // Nasze nowe ID dla xdg_toplevel
        let msg_top = WaylandFrame::new(xdg_surface, 1, enc_top.get_buffer());
        self.stream.write_all(&msg_top.serialize())?;
        Ok(id)
    }

    fn wl_curface_commit(&mut self, wl_surface_id: u32) -> std::io::Result<()> {
        let enc_commit = FrameEncoder::new();
        let msg_commit = WaylandFrame::new(wl_surface_id, 6, enc_commit.get_buffer());
        self.stream.write_all(&msg_commit.serialize())?;
        Ok(())
    }

    fn event_pool(&self) -> std::io::Result<WaylandEvent> {
        todo!();
    }
}
fn main() {
    // test
    let mut test_connection = WaylandConnect::init().unwrap();
    test_connection.bind_registry();
    test_connection.sync();
    test_connection.sync();
    println!("=== Test refactor ===");
    // connect to socket
    let mut stream = connect_to_socket().unwrap();

    // ID for gloabls
    let mut compositor_name: Option<u32> = None;
    let mut shm_name: Option<u32> = None;
    let mut xdg_wm_base_name: Option<u32> = None;
    let mut state = StateMachine::SendGetRegistry;

    // configure parameters
    let mut serial: Option<u32> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    loop {
        match state {
            StateMachine::SendGetRegistry => {
                // get registry
                let mut new_request = FrameEncoder::new();
                new_request.write_uint(2);
                let new_message = WaylandFrame::new(1, 1, new_request.get_buffer());
                stream.write_all(&new_message.serialize()).unwrap();
                stream.flush().unwrap();

                // SYNC
                let mut sync = FrameEncoder::new();
                sync.write_uint(3);
                let sync_message = WaylandFrame::new(1, 0, sync.get_buffer());
                println!("{:?}", &sync_message.serialize());
                stream.write_all(&sync_message.serialize()).unwrap();
                stream.flush().unwrap();
                state = StateMachine::GetRegistry;
            }
            StateMachine::GetRegistry => {
                // read message
                let read_message = WaylandFrame::read_from_stream(&mut stream).unwrap();
                let current_id = read_message.id;

                // error from get_registry
                if current_id == 3 {
                    break;
                }
                // getting arguments from wl_registry

                let mut decoder = FrameDecoder::new(read_message.arguments);
                let name = decoder.read_uint();
                let interface = decoder.read_string();
                if interface == "wl_compositor" {
                    compositor_name = Some(name);
                } else if interface == "wl_shm" {
                    shm_name = Some(name);
                } else if interface == "xdg_wm_base" {
                    xdg_wm_base_name = Some(name);
                }
                let version = decoder.read_uint();

                println!("name: {name} \n interface: {interface} \n version: {version}");
                if xdg_wm_base_name != None && shm_name != None && compositor_name != None {
                    state = StateMachine::BindLocalID;
                }
            }
            StateMachine::BindLocalID => {
                dbg!("wejście do bindowania");
                let comp_num = compositor_name.expect("Brak compositora");
                let shm_num = shm_name.expect("Brak SHM");
                let xdg_num = xdg_wm_base_name.expect("Brak XDG WM Base");
                // set local id for objects

                // Bind to wl_compositor (ID = 4)
                let mut enc1 = FrameEncoder::new();
                enc1.write_uint(comp_num);
                enc1.write_string("wl_compositor");
                enc1.write_uint(6);
                enc1.write_uint(WL_COMPOSITOR);
                let msg1 = WaylandFrame::new(2, 0, enc1.get_buffer());
                println!("wl_compositor: {:?}", &msg1.serialize());
                stream.write_all(&msg1.serialize()).unwrap();

                // Bind to wl_shm (lokalne ID = 5)
                let mut enc2 = FrameEncoder::new();
                enc2.write_uint(shm_num);
                enc2.write_string("wl_shm");
                enc2.write_uint(2);
                enc2.write_uint(WL_SHM);
                let msg2 = WaylandFrame::new(2, 0, enc2.get_buffer());
                stream.write_all(&msg2.serialize()).unwrap();

                // Bind to xdg_wm_base (lokalne ID = 6)
                let mut enc3 = FrameEncoder::new();
                enc3.write_uint(xdg_num);
                enc3.write_string("xdg_wm_base");
                enc3.write_uint(7);
                enc3.write_uint(XDG_WM_BASE);
                let msg3 = WaylandFrame::new(2, 0, enc3.get_buffer());
                stream.write_all(&msg3.serialize()).unwrap();

                // SYNC
                let mut sync = FrameEncoder::new();
                sync.write_uint(7);
                let sync_message = WaylandFrame::new(1, 0, sync.get_buffer());
                println!("SYNC: {:?}", &sync_message.serialize());
                stream.write_all(&sync_message.serialize()).unwrap();
                stream.flush().unwrap();
                state = StateMachine::WaitToBind;
            }
            StateMachine::WaitToBind => {
                let sync = WaylandFrame::read_from_stream(&mut stream).unwrap();
                println!("{} {}, {:?}", sync.id, sync.opcode, sync.arguments);
                if sync.id == 7 {
                    println!("odebrano pakiet commit ");
                    state = StateMachine::CreateSurface;
                }
            }
            StateMachine::CreateSurface => {
                // Create wl_surface (ID = 8)
                let mut enc_surf = FrameEncoder::new();
                enc_surf.write_uint(WL_SURFACE);
                let msg_surf = WaylandFrame::new(WL_COMPOSITOR, 0, enc_surf.get_buffer());
                stream.write_all(&msg_surf.serialize()).unwrap();
                // SYNC
                let mut sync = FrameEncoder::new();
                sync.write_uint(9);
                let sync_message = WaylandFrame::new(1, 0, sync.get_buffer());
                println!("SYNC: {:?}", &sync_message.serialize());
                stream.write_all(&sync_message.serialize()).unwrap();
                stream.flush().unwrap();
                state = StateMachine::WaitToXDG;
            }
            StateMachine::WaitToXDG => {
                let sync = WaylandFrame::read_from_stream(&mut stream).unwrap();
                println!("{} {}, {:?}", sync.id, sync.opcode, sync.arguments);
                if sync.id == 9 {
                    println!("stworzono WL_surface");
                    state = StateMachine::CreateXDG;
                }
            }
            StateMachine::CreateXDG => {
                //  xdg_surface (ID = 10) from wl_surface (ID = 8) na xdg_wm_base (ID = 6)
                let mut enc_xdg = FrameEncoder::new();
                enc_xdg.write_uint(XDG_SURFACE); // Nasze nowe ID dla xdg_surface
                enc_xdg.write_uint(WL_SURFACE); // Istniejąca wl_surface
                let msg_xdg = WaylandFrame::new(XDG_WM_BASE, 2, enc_xdg.get_buffer());
                stream.write_all(&msg_xdg.serialize()).unwrap();
                // SYNC
                let mut sync = FrameEncoder::new();
                sync.write_uint(11);
                let sync_message = WaylandFrame::new(1, 0, sync.get_buffer());
                println!("SYNC: {:?}", &sync_message.serialize());
                stream.write_all(&sync_message.serialize()).unwrap();
                stream.flush().unwrap();
                state = StateMachine::WaitToToplevel;
            }
            StateMachine::WaitToToplevel => {
                let sync = WaylandFrame::read_from_stream(&mut stream).unwrap();
                println!("{} {}, {:?}", sync.id, sync.opcode, sync.arguments);
                if sync.id == 11 {
                    println!("stworzono XDG_surface");
                    // Create xdg_toplevel (ID = 12) from xdg_surface (ID = 10)
                    let mut enc_top = FrameEncoder::new();
                    enc_top.write_uint(XDG_TOPLEVEL); // Nasze nowe ID dla xdg_toplevel
                    let msg_top = WaylandFrame::new(XDG_SURFACE, 1, enc_top.get_buffer());
                    stream.write_all(&msg_top.serialize()).unwrap();

                    // Committing changes form wl_sufrace
                    let enc_commit = FrameEncoder::new();
                    let msg_commit = WaylandFrame::new(WL_SURFACE, 6, enc_commit.get_buffer());
                    stream.write_all(&msg_commit.serialize()).unwrap();
                    stream.flush().unwrap();
                    state = StateMachine::Configure;
                }
            }

            StateMachine::Configure => {
                let verification_frame = WaylandFrame::read_from_stream(&mut stream).unwrap();
                println!(
                    "ID: {} Opcode: {} Arg: {:?}",
                    verification_frame.id, verification_frame.opcode, verification_frame.arguments
                );
                let object_id = verification_frame.id;
                let opcode = verification_frame.opcode;
                let arg = verification_frame.arguments;
                if object_id == 12 && opcode == 0 {
                    // configure event
                    width = Some(u32::from_ne_bytes(arg[0..4].try_into().unwrap()));
                    height = Some(u32::from_ne_bytes(arg[4..8].try_into().unwrap()));
                    println!("Kompozytor chce okna {:?} x {:?}", width, height);
                }
                if object_id == 10 && opcode == 0 {
                    serial = Some(u32::from_ne_bytes(arg[0..4].try_into().unwrap()));

                    // ACK configure
                    let mut enc_ack = FrameEncoder::new();
                    enc_ack.write_uint(serial.unwrap());
                    let msg_ack = WaylandFrame::new(XDG_SURFACE, 4, enc_ack.get_buffer());
                    stream.write_all(&msg_ack.serialize()).unwrap();

                    // commit
                    let msg_commit = WaylandFrame::new(WL_SURFACE, 6, vec![0u8; 0]);
                    stream.write_all(&msg_commit.serialize()).unwrap();
                    stream.flush().unwrap();
                }
                if width != None && height != None && serial != None {
                    println!("{:?}", serial);
                    state = StateMachine::MainLoop;
                }
            }
            StateMachine::MainLoop => {
                println!("In Mainloop");
                let size = (width.unwrap() * height.unwrap()) as usize * 4;
                let buffer = WaylandBuffer::new(size).unwrap();

                let pixels = unsafe { std::slice::from_raw_parts_mut(buffer.as_mut_ptr(), size) };

                for chunk in pixels.chunks_exact_mut(4) {
                    chunk[0] = 255; // B
                    chunk[1] = 0; // G
                    chunk[2] = 0; // R
                    chunk[3] = 255; // A
                }

                // Tworzenie puli SHM (create_pool na obiekcie wl_shm [5])
                let mut enc_pool = FrameEncoder::new();
                enc_pool.write_uint(SHM_POOL); // ID nowej puli = 11
                enc_pool.write_uint(size as u32);
                let serialized = WaylandFrame::new(WL_SHM, 0, enc_pool.get_buffer()).serialize();

                // Przygotowanie iovec dla danych
                let mut iov = libc::iovec {
                    iov_base: serialized.as_ptr() as *mut libc::c_void,
                    iov_len: serialized.len(),
                };

                // Prawidłowe i bezpiecznie wyrównane miejsce na dane kontrolne (fd)
                // Gwarantujemy wyrównanie 64-bitowe za pomocą typu u64
                let cmsg_space =
                    unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as usize };
                let u64_count = (cmsg_space + 7) / 8;
                let mut control_buf = vec![0u64; u64_count];

                unsafe {
                    let mut msg: libc::msghdr = std::mem::zeroed();
                    msg.msg_name = std::ptr::null_mut();
                    msg.msg_namelen = 0;
                    msg.msg_iov = &mut iov as *mut libc::iovec;
                    msg.msg_iovlen = 1;
                    msg.msg_control = control_buf.as_mut_ptr() as *mut libc::c_void;
                    msg.msg_controllen = u64_count * 8;

                    let cmsg = libc::CMSG_FIRSTHDR(&msg);
                    if !cmsg.is_null() {
                        (*cmsg).cmsg_len =
                            libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as usize;
                        (*cmsg).cmsg_level = libc::SOL_SOCKET;
                        (*cmsg).cmsg_type = libc::SCM_RIGHTS;

                        let fd_ptr = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
                        *fd_ptr = buffer.get_fd();
                    }

                    println!("Wysyłam pulę pamięci SHM przez gniazdo za pomocą libc::sendmsg...");
                    let bytes_sent = libc::sendmsg(stream.as_raw_fd(), &msg, 0);

                    if bytes_sent < 0 {
                        let errno = *libc::__errno_location();
                        panic!(
                            "Błąd sendmsg! Kod powrotu: {}. Systemowy numer błędu (errno): {}.",
                            bytes_sent, errno
                        );
                    } else {
                        println!(
                            "Sukces! sendmsg wysłało {} bajtów. Pula pamięci (shm_pool) została pomyślnie utworzona na serwerze Wayland!",
                            bytes_sent
                        );
                    }
                };
                let mut enc_buf = FrameEncoder::new();
                enc_buf.write_uint(WL_BUFFER); // ID naszego nowego wl_buffer
                enc_buf.write_int(0); // offset w pamięci (zaczynamy od zera)
                enc_buf.write_int(width.unwrap() as i32);
                enc_buf.write_int(height.unwrap() as i32);
                enc_buf.write_int((width.unwrap() * 4) as i32); // stride (bajty na linię)
                enc_buf.write_uint(0); // Format: 0 reprezentuje zazwyczaj WL_SHM_FORMAT_XRGB8888

                let msg_buf = WaylandFrame::new(SHM_POOL, 0, enc_buf.get_buffer());
                stream.write_all(&msg_buf.serialize()).unwrap();

                let mut enc_attach = FrameEncoder::new();
                enc_attach.write_uint(WL_BUFFER); // ID naszego wl_buffer
                enc_attach.write_int(0); // x offset na ekranie
                enc_attach.write_int(0); // y offset na ekranie
                let msg_attach = WaylandFrame::new(WL_SURFACE, 1, enc_attach.get_buffer());
                stream.write_all(&msg_attach.serialize()).unwrap();

                let mut enc_damage = FrameEncoder::new();
                enc_damage.write_int(0);
                enc_damage.write_int(0);
                enc_damage.write_int(width.unwrap() as i32);
                enc_damage.write_int(height.unwrap() as i32);
                let msg_damage = WaylandFrame::new(WL_SURFACE, 2, enc_damage.get_buffer());
                stream.write_all(&msg_damage.serialize()).unwrap();

                let msg_commit = WaylandFrame::new(WL_SURFACE, 6, vec![]);
                stream.write_all(&msg_commit.serialize()).unwrap();
                stream.flush().unwrap();
                loop {
                    let frame = WaylandFrame::read_from_stream(&mut stream).unwrap();
                    println!("{:?}", frame);
                    match frame.id {
                        XDG_WM_BASE => {
                            let ping_serial =
                                u32::from_ne_bytes(frame.arguments[0..4].try_into().unwrap());
                            let mut enc_pong = FrameEncoder::new();
                            enc_pong.write_uint(ping_serial);
                            let msg_pong = WaylandFrame::new(XDG_WM_BASE, 3, enc_pong.get_buffer());
                            stream.write_all(&msg_pong.serialize()).unwrap();
                            stream.flush().unwrap();
                        }
                        XDG_TOPLEVEL => {
                            if frame.opcode == 1 {
                                break;
                            }
                        }
                        _ => {}
                    };
                }
            }
        };
    }
}
