//! Wayland cursor utilities
//!
//! This crate aims to reimplement the functionality of the `libwayland-cursor` library in Rust.
//!
//! It allows you to load cursors from the system and display them correctly.
//!
//! First of all, you need to create a `CursorTheme`,
//! which represents the full cursor theme.
//!
//! From this theme, using the `get_cursor` method, you can load a specific `Cursor`,
//! which can contain several images if the cursor is animated. It also provides you with the
//! means of querying which frame of the animation should be displayed at
//! what time, as well as handles to the buffers containing these frames, to
//! attach them to a wayland surface.
//!
//! # Example
//!
//! ```ignore
//! use wayland_cursor::CursorTheme;
//! # use std::thread::sleep;
//! # use std::time::{Instant, Duration};
//!
//! let cursor_theme = CursorTheme::load(32, wl_shm);
//! let cursor = cursor_theme.get_cursor("wait").expect("Cursor not provided by theme");
//!
//! let start_time = Instant::now();
//! loop {
//!     // Obtain which frame we should show, and for how long.
//!     let millis = start_time.elapsed().as_millis();
//!     let fr_info = cursor.frame_and_duration(millis as u32);
//!
//!     // Here, we obtain the right cursor frame...
//!     let buffer = cursor[fr_info.frame_index];
//!     // and attach it to a wl_surface.
//!     cursor_surface.attach(Some(&buffer), 0, 0);
//!     cursor_surface.commit();
//!
//!     sleep(fr_info.frame_duration);
//! }
//! ```

use std::{
    env,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    ops::{Deref, Index},
    os::unix::io::{AsRawFd, FromRawFd},
};
use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
    },
    Attached, Main,
};
use xcur::parser::File as XCurFile;
use xcursor::{theme_search_paths, XCursorTheme};

/// Represents a cursor theme loaded from the system.
pub struct CursorTheme {
    name: String,
    cursors: Vec<Cursor>,
    size: u32,
    pool: Main<WlShmPool>,
    pool_size: i32,
    file: File,
}

impl CursorTheme {
    /// Load a cursor theme from system defaults.
    ///
    /// Same as calling `load_or("default", size, shm)`
    pub fn load(size: u32, shm: &Attached<WlShm>) -> Self {
        CursorTheme::load_or("default", size, shm)
    }

    /// Load a cursor theme, using `name` as fallback.
    ///
    /// The theme name and cursor size are read from the `XCURSOR_THEME` and
    /// `XCURSOR_SIZE` environment variables, respectively, or from the provided variables
    /// if those are invalid.
    pub fn load_or(name: &str, mut size: u32, shm: &Attached<WlShm>) -> Self {
        let name_string = String::from(name);
        let name = &env::var("XCURSOR_THEME").unwrap_or(name_string);

        if let Ok(var) = env::var("XCURSOR_SIZE") {
            if let Ok(int) = var.parse() {
                size = int;
            }
        }

        CursorTheme::load_from_name(name, size, shm)
    }

    /// Create a new cursor theme, ignoring the system defaults.
    pub fn load_from_name(name: &str, size: u32, shm: &Attached<WlShm>) -> Self {
        let name = String::from(name);
        let pool_size = (size * size * 4) as i32;
        let mem_fd = create_shm_fd().unwrap();
        let file = unsafe { File::from_raw_fd(mem_fd) };
        let pool = shm.create_pool(file.as_raw_fd(), pool_size);

        CursorTheme {
            name,
            file,
            size,
            pool,
            pool_size,
            cursors: Vec::new(),
        }
    }

    /// Retrieve a cursor from the theme.
    ///
    /// This method returns `None` if this cursor is not provided
    /// either by the theme, or by one of its parents.
    pub fn get_cursor(&mut self, name: &str) -> Option<&Cursor> {
        let cur = self.cursors.iter().position(|i| i.name == name);

        match cur {
            Some(i) => Some(&self.cursors[i]),
            None => {
                let cur = self.load_cursor(name, self.size)?;
                self.cursors.push(cur);
                self.cursors.iter().last()
            }
        }
    }

    /// This function loads a cursor, parses it and
    /// pushes the images onto the shm pool.
    /// Keep in mind that if the cursor is already loaded,
    /// the function will make a duplicate.
    fn load_cursor(&mut self, name: &str, size: u32) -> Option<Cursor> {
        let icon_path = XCursorTheme::load(&self.name, &theme_search_paths()).load_icon(name)?;
        let mut icon_file = File::open(icon_path).ok()?;

        let mut buf = Vec::new();
        let xcur = {
            icon_file.read_to_end(&mut buf).ok()?;
            XCurFile::parse(&buf)
        };

        // Terminate if cursor can't be parsed
        if !xcur.is_done() {
            return None;
        }

        let file_images = xcur.unwrap().1.images;
        let cursor = Cursor::new(name, self, &file_images, size);

        Some(cursor)
    }

    /// Grow the wl_shm_pool this theme is stored on.
    /// This method does nothing if the provided size is
    /// smaller or equal to the pool's current size.
    fn grow(&mut self, size: i32) {
        if size > self.pool_size {
            self.pool.resize(size);
            self.pool_size = size;
        }
    }
}

/// A cursor from a theme. Can contain several images if animated.
#[derive(Clone)]
pub struct Cursor {
    name: String,
    images: Vec<CursorImageBuffer>,
    total_duration: u32,
}

impl Cursor {
    /// Construct a new Cursor.
    ///
    /// Each of the provided images will be written into `theme`.
    /// This will also grow `theme.pool` if necessary.
    fn new(name: &str, theme: &mut CursorTheme, images: &[xcur::parser::Image], size: u32) -> Self {
        let mut buffers = Vec::with_capacity(images.len());
        let size = Cursor::nearest_size(size, images);
        let iter = images.iter().filter(|el| el.width == size && el.height == size);

        for img in iter {
            buffers.push(CursorImageBuffer::new(theme, img));
        }

        let total_duration = buffers.iter().map(|el| el.delay).sum();

        Cursor {
            total_duration,
            name: String::from(name),
            images: buffers,
        }
    }

    fn nearest_size(size: u32, images: &[xcur::parser::Image]) -> u32 {
        let size = size as i32;
        let mut all_sizes = Vec::new();

        for img in images {
            if !all_sizes.contains(&(img.width as i32)) {
                all_sizes.push(img.width as i32);
            }
        }

        let mut min = 0;
        for (i, width) in all_sizes.iter().enumerate() {
            if (width - size).abs() < (all_sizes[min] - size).abs() {
                min = i;
            }
        }
        all_sizes[min] as u32
    }

    /// Given a time, calculate which frame to show, and how much time remains until the next frame.
    ///
    /// Time will wrap, so if for instance the cursor has an animation during 100ms,
    /// then calling this function with 5ms and 105ms as input gives the same output.
    pub fn frame_and_duration(&self, mut millis: u32) -> FrameAndDuration {
        millis %= self.total_duration;

        let mut res = 0;
        for (i, img) in self.images.iter().enumerate() {
            if millis < img.delay {
                res = i;
                break;
            }
            millis -= img.delay;
        }

        FrameAndDuration {
            frame_index: res,
            frame_duration: millis,
        }
    }

    /// Total number of images forming this cursor animation
    pub fn image_count(&self) -> usize {
        self.images.len()
    }
}

impl Index<usize> for Cursor {
    type Output = CursorImageBuffer;

    fn index(&self, index: usize) -> &Self::Output {
        &self.images[index]
    }
}

/// A buffer containing a cursor image.
///
/// You can access the `WlBuffer` via `Deref`.
///
/// Note that this proxy will be considered as "unmanaged" by the crate, as such you should
/// not try to act on it beyond assigning it to `wl_surface`s.
#[derive(Clone)]
pub struct CursorImageBuffer {
    buffer: WlBuffer,
    delay: u32,
    xhot: u32,
    yhot: u32,
    width: u32,
    height: u32,
}

impl CursorImageBuffer {
    /// Construct a new CursorImageBuffer
    ///
    /// This function appends the pixels of the image to the provided file,
    /// and constructs a wl_buffer on that data.
    fn new(theme: &mut CursorTheme, image: &xcur::parser::Image) -> Self {
        let buf = CursorImageBuffer::convert_pixels(&image.pixels);
        let offset = theme.file.seek(SeekFrom::End(0)).unwrap();
        theme.file.write_all(&buf).unwrap();

        let new_size = theme.file.seek(SeekFrom::End(0)).unwrap();
        theme.grow(new_size as i32);

        let buffer = theme.pool.create_buffer(
            offset as i32,
            image.width as i32,
            image.height as i32,
            (image.width * 4) as i32,
            Format::Argb8888,
        );
        buffer.quick_assign(|_, _, _| {});

        CursorImageBuffer {
            buffer: buffer.detach(),
            delay: image.delay,
            xhot: image.xhot,
            yhot: image.yhot,
            width: image.width,
            height: image.height,
        }
    }

    /// Convert the pixels saved in `u32`s into `u8`s.
    fn convert_pixels(pixels: &[u32]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::with_capacity(pixels.len() * 4);

        for pixel in pixels {
            buf.extend_from_slice(&pixel.to_le_bytes());
        }

        buf
    }

    /// Dimensions of this image
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Location of the pointer hotspot in this image
    pub fn hotspot(&self) -> (u32, u32) {
        (self.xhot, self.yhot)
    }

    /// Time (in milliseconds) for which this image should be displayed
    pub fn delay(&self) -> u32 {
        self.delay
    }
}

impl Deref for CursorImageBuffer {
    type Target = WlBuffer;
    fn deref(&self) -> &WlBuffer {
        &self.buffer
    }
}

/// Which frame to show, and for how long.
///
/// This struct is output by `Cursor::frame_and_duration`
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FrameAndDuration {
    /// The index of the frame which should be shown.
    pub frame_index: usize,
    /// The duration that the frame should be shown for (in milliseconds).
    pub frame_duration: u32,
}

/// Create a shared file descriptor in memory
use {
    nix::{
        errno::Errno,
        fcntl,
        sys::{mman, stat},
        unistd,
    },
    std::{
        io,
        os::unix::io::RawFd,
        time::{SystemTime, UNIX_EPOCH},
    },
};

fn create_shm_fd() -> io::Result<RawFd> {
    // Only try memfd on linux
    #[cfg(target_os = "linux")]
    loop {
        use {nix::sys::memfd, std::ffi::CStr};
        match memfd::memfd_create(
            CStr::from_bytes_with_nul(b"wayland-cursor-rs\0").unwrap(),
            memfd::MemFdCreateFlag::MFD_CLOEXEC,
        ) {
            Ok(fd) => return Ok(fd),
            Err(nix::Error::Sys(Errno::EINTR)) => continue,
            Err(nix::Error::Sys(Errno::ENOSYS)) => break,
            Err(nix::Error::Sys(errno)) => return Err(io::Error::from(errno)),
            Err(err) => unreachable!(err),
        }
    }

    // Fallback to using shm_open
    let sys_time = SystemTime::now();
    let mut mem_file_handle = format!(
        "/wayland-cursor-rs-{}",
        sys_time.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
    );
    loop {
        match mman::shm_open(
            mem_file_handle.as_str(),
            fcntl::OFlag::O_CREAT | fcntl::OFlag::O_EXCL | fcntl::OFlag::O_RDWR | fcntl::OFlag::O_CLOEXEC,
            stat::Mode::S_IRUSR | stat::Mode::S_IWUSR,
        ) {
            Ok(fd) => match mman::shm_unlink(mem_file_handle.as_str()) {
                Ok(_) => return Ok(fd),
                Err(nix::Error::Sys(errno)) => match unistd::close(fd) {
                    Ok(_) => return Err(io::Error::from(errno)),
                    Err(nix::Error::Sys(errno)) => return Err(io::Error::from(errno)),
                    Err(err) => panic!(err),
                },
                Err(err) => panic!(err),
            },
            Err(nix::Error::Sys(Errno::EEXIST)) => {
                // If a file with that handle exists then change the handle
                mem_file_handle = format!(
                    "/wayland-cursor-rs-{}",
                    sys_time.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
                );
                continue;
            }
            Err(nix::Error::Sys(Errno::EINTR)) => continue,
            Err(nix::Error::Sys(errno)) => return Err(io::Error::from(errno)),
            Err(err) => unreachable!(err),
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_convert_pixels() {
        let pixels: &[u32] = &[0x12345678, 0x87654321];
        let parsed_pixels: &[u8] = &[0x78, 0x56, 0x34, 0x12, 0x21, 0x43, 0x65, 0x87];

        assert_eq!(
            super::CursorImageBuffer::convert_pixels(&pixels),
            Vec::from(parsed_pixels)
        );
    }
}
