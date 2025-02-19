//! Implementation of software buffering for Windows.
//!
//! This module converts the input buffer into a bitmap and then stretches it to the window.

use crate::{Rect, SoftBufferError};
use raw_window_handle::Win32WindowHandle;

use std::io;
use std::mem;
use std::num::{NonZeroI32, NonZeroU32};
use std::ptr::{self, NonNull};
use std::slice;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi;

const ZERO_QUAD: Gdi::RGBQUAD = Gdi::RGBQUAD {
    rgbBlue: 0,
    rgbGreen: 0,
    rgbRed: 0,
    rgbReserved: 0,
};

struct Buffer {
    dc: Gdi::HDC,
    bitmap: Gdi::HBITMAP,
    pixels: NonNull<u32>,
    width: NonZeroI32,
    height: NonZeroI32,
    presented: bool,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            Gdi::DeleteDC(self.dc);
            Gdi::DeleteObject(self.bitmap);
        }
    }
}

impl Buffer {
    fn new(window_dc: Gdi::HDC, width: NonZeroI32, height: NonZeroI32) -> Self {
        let dc = unsafe { Gdi::CreateCompatibleDC(window_dc) };
        assert!(dc != 0);

        // Create a new bitmap info struct.
        let bitmap_info = BitmapInfo {
            bmi_header: Gdi::BITMAPINFOHEADER {
                biSize: mem::size_of::<Gdi::BITMAPINFOHEADER>() as u32,
                biWidth: width.get(),
                biHeight: -height.get(),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: Gdi::BI_BITFIELDS as u32,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmi_colors: [
                Gdi::RGBQUAD {
                    rgbRed: 0xff,
                    ..ZERO_QUAD
                },
                Gdi::RGBQUAD {
                    rgbGreen: 0xff,
                    ..ZERO_QUAD
                },
                Gdi::RGBQUAD {
                    rgbBlue: 0xff,
                    ..ZERO_QUAD
                },
            ],
        };

        // XXX alignment?
        // XXX better to use CreateFileMapping, and pass hSection?
        // XXX test return value?
        let mut pixels: *mut u32 = ptr::null_mut();
        let bitmap = unsafe {
            Gdi::CreateDIBSection(
                dc,
                &bitmap_info as *const BitmapInfo as *const _,
                Gdi::DIB_RGB_COLORS,
                &mut pixels as *mut *mut u32 as _,
                0,
                0,
            )
        };
        assert!(bitmap != 0);
        let pixels = NonNull::new(pixels).unwrap();

        unsafe {
            Gdi::SelectObject(dc, bitmap);
        }

        Self {
            dc,
            bitmap,
            width,
            height,
            pixels,
            presented: false,
        }
    }

    #[inline]
    fn pixels(&self) -> &[u32] {
        unsafe {
            slice::from_raw_parts(
                self.pixels.as_ptr(),
                i32::from(self.width) as usize * i32::from(self.height) as usize,
            )
        }
    }

    #[inline]
    fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe {
            slice::from_raw_parts_mut(
                self.pixels.as_ptr(),
                i32::from(self.width) as usize * i32::from(self.height) as usize,
            )
        }
    }
}

/// The handle to a window for software buffering.
pub struct Win32Impl {
    /// The window handle.
    window: HWND,

    /// The device context for the window.
    dc: Gdi::HDC,

    /// The buffer used to hold the image.
    buffer: Option<Buffer>,
}

/// The Win32-compatible bitmap information.
#[repr(C)]
struct BitmapInfo {
    bmi_header: Gdi::BITMAPINFOHEADER,
    bmi_colors: [Gdi::RGBQUAD; 3],
}

impl Win32Impl {
    /// Create a new `Win32Impl` from a `Win32WindowHandle`.
    ///
    /// # Safety
    ///
    /// The `Win32WindowHandle` must be a valid window handle.
    pub unsafe fn new(handle: &Win32WindowHandle) -> Result<Self, crate::SoftBufferError> {
        // It is valid for the window handle to be null here. Error out if it is.
        if handle.hwnd.is_null() {
            return Err(SoftBufferError::IncompleteWindowHandle);
        }

        // Get the handle to the device context.
        // SAFETY: We have confirmed that the window handle is valid.
        let hwnd = handle.hwnd as HWND;
        let dc = unsafe { Gdi::GetDC(hwnd) };

        // GetDC returns null if there is a platform error.
        if dc == 0 {
            return Err(SoftBufferError::PlatformError(
                Some("Device Context is null".into()),
                Some(Box::new(io::Error::last_os_error())),
            ));
        }

        Ok(Self {
            dc,
            window: hwnd,
            buffer: None,
        })
    }

    pub fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) -> Result<(), SoftBufferError> {
        let (width, height) = (|| {
            let width = NonZeroI32::new(i32::try_from(width.get()).ok()?)?;
            let height = NonZeroI32::new(i32::try_from(height.get()).ok()?)?;
            Some((width, height))
        })()
        .ok_or(SoftBufferError::SizeOutOfRange { width, height })?;

        if let Some(buffer) = self.buffer.as_ref() {
            if buffer.width == width && buffer.height == height {
                return Ok(());
            }
        }

        self.buffer = Some(Buffer::new(self.dc, width, height));

        Ok(())
    }

    pub fn buffer_mut(&mut self) -> Result<BufferImpl, SoftBufferError> {
        if self.buffer.is_none() {
            panic!("Must set size of surface before calling `buffer_mut()`");
        }

        Ok(BufferImpl(self))
    }

    fn present_with_damage(&mut self, damage: &[Rect]) -> Result<(), SoftBufferError> {
        let buffer = self.buffer.as_mut().unwrap();
        unsafe {
            for rect in damage.iter().copied() {
                let (x, y, width, height) = (|| {
                    Some((
                        i32::try_from(rect.x).ok()?,
                        i32::try_from(rect.y).ok()?,
                        i32::try_from(rect.width.get()).ok()?,
                        i32::try_from(rect.height.get()).ok()?,
                    ))
                })()
                .ok_or(SoftBufferError::DamageOutOfRange { rect })?;
                Gdi::BitBlt(self.dc, x, y, width, height, buffer.dc, x, y, Gdi::SRCCOPY);
            }

            // Validate the window.
            Gdi::ValidateRect(self.window, ptr::null_mut());
        }
        buffer.presented = true;

        Ok(())
    }

    /// Fetch the buffer from the window.
    pub fn fetch(&mut self) -> Result<Vec<u32>, SoftBufferError> {
        let buffer = self.buffer.as_ref().unwrap();
        let temp_buffer = Buffer::new(self.dc, buffer.width, buffer.height);

        // Just go the other way.
        unsafe {
            Gdi::BitBlt(
                temp_buffer.dc,
                0,
                0,
                temp_buffer.width.get(),
                temp_buffer.height.get(),
                self.dc,
                0,
                0,
                Gdi::SRCCOPY,
            );
        }

        // Flush the operation so that it happens immediately.
        unsafe {
            Gdi::GdiFlush();
        }

        Ok(temp_buffer.pixels().to_vec())
    }
}

pub struct BufferImpl<'a>(&'a mut Win32Impl);

impl<'a> BufferImpl<'a> {
    #[inline]
    pub fn pixels(&self) -> &[u32] {
        self.0.buffer.as_ref().unwrap().pixels()
    }

    #[inline]
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.0.buffer.as_mut().unwrap().pixels_mut()
    }

    pub fn age(&self) -> u8 {
        match self.0.buffer.as_ref() {
            Some(buffer) if buffer.presented => 1,
            _ => 0,
        }
    }

    pub fn present(self) -> Result<(), SoftBufferError> {
        let imp = self.0;
        let buffer = imp.buffer.as_ref().unwrap();
        imp.present_with_damage(&[Rect {
            x: 0,
            y: 0,
            // We know width/height will be non-negative
            width: buffer.width.try_into().unwrap(),
            height: buffer.height.try_into().unwrap(),
        }])
    }

    pub fn present_with_damage(self, damage: &[Rect]) -> Result<(), SoftBufferError> {
        let imp = self.0;
        imp.present_with_damage(damage)
    }
}
