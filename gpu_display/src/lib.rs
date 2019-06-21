// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Crate for displaying simple surfaces and GPU buffers over wayland.

mod dwl;

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fmt::{self, Display};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::ptr::null_mut;

use data_model::{VolatileMemory, VolatileSlice};
use sys_util::{round_up_to_page_size, Error as SysError, MemoryMapping, SharedMemory};

use crate::dwl::*;

const BUFFER_COUNT: usize = 2;
const BYTES_PER_PIXEL: u32 = 4;

/// An error generated by `GpuDisplay`.
#[derive(Debug)]
pub enum GpuDisplayError {
    /// An internal allocation failed.
    Allocate,
    /// Connecting to the compositor failed.
    Connect,
    /// Creating shared memory failed.
    CreateShm(SysError),
    /// Setting the size of shared memory failed.
    SetSize(SysError),
    /// Failed to create a surface on the compositor.
    CreateSurface,
    /// Failed to import a buffer to the compositor.
    FailedImport,
    /// The surface ID is invalid.
    InvalidSurfaceId,
    /// The path is invalid.
    InvalidPath,
}

impl Display for GpuDisplayError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::GpuDisplayError::*;

        match self {
            Allocate => write!(f, "internal allocation failed"),
            Connect => write!(f, "failed to connect to compositor"),
            CreateShm(e) => write!(f, "failed to create shared memory: {}", e),
            SetSize(e) => write!(f, "failed to set size of shared memory: {}", e),
            CreateSurface => write!(f, "failed to crate surface on the compositor"),
            FailedImport => write!(f, "failed to import a buffer to the compositor"),
            InvalidSurfaceId => write!(f, "invalid surface ID"),
            InvalidPath => write!(f, "invalid path"),
        }
    }
}

struct DwlContext(*mut dwl_context);
impl Drop for DwlContext {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safe given that we checked the pointer for non-null and it should always be of the
            // correct type.
            unsafe {
                dwl_context_destroy(&mut self.0);
            }
        }
    }
}

struct DwlDmabuf(*mut dwl_dmabuf);
impl Drop for DwlDmabuf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safe given that we checked the pointer for non-null and it should always be of the
            // correct type.
            unsafe {
                dwl_dmabuf_destroy(&mut self.0);
            }
        }
    }
}

struct DwlSurface(*mut dwl_surface);
impl Drop for DwlSurface {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safe given that we checked the pointer for non-null and it should always be of the
            // correct type.
            unsafe {
                dwl_surface_destroy(&mut self.0);
            }
        }
    }
}

struct GpuDisplaySurface {
    surface: DwlSurface,
    buffer_size: usize,
    buffer_index: Cell<usize>,
    buffer_mem: MemoryMapping,
}

impl GpuDisplaySurface {
    fn surface(&self) -> *mut dwl_surface {
        self.surface.0
    }
}

/// A connection to the compositor and associated collection of state.
///
/// The user of `GpuDisplay` can use `AsRawFd` to poll on the compositor connection's file
/// descriptor. When the connection is readable, `dispatch_events` can be called to process it.
pub struct GpuDisplay {
    ctx: DwlContext,
    dmabufs: HashMap<u32, DwlDmabuf>,
    dmabuf_next_id: u32,
    surfaces: HashMap<u32, GpuDisplaySurface>,
    surface_next_id: u32,
}

impl GpuDisplay {
    /// Opens a fresh connection to the compositor.
    pub fn new<P: AsRef<Path>>(wayland_path: P) -> Result<GpuDisplay, GpuDisplayError> {
        // The dwl_context_new call should always be safe to call, and we check its result.
        let ctx = DwlContext(unsafe { dwl_context_new() });
        if ctx.0.is_null() {
            return Err(GpuDisplayError::Allocate);
        }

        // The dwl_context_setup call is always safe to call given that the supplied context is
        // valid. and we check its result.
        let cstr_path = match wayland_path.as_ref().as_os_str().to_str() {
            Some(str) => match CString::new(str) {
                Ok(cstr) => cstr,
                Err(_) => return Err(GpuDisplayError::InvalidPath),
            },
            None => return Err(GpuDisplayError::InvalidPath),
        };
        let setup_success = unsafe { dwl_context_setup(ctx.0, cstr_path.as_ptr()) };
        if !setup_success {
            return Err(GpuDisplayError::Connect);
        }

        Ok(GpuDisplay {
            ctx,
            dmabufs: Default::default(),
            dmabuf_next_id: 0,
            surfaces: Default::default(),
            surface_next_id: 0,
        })
    }

    fn ctx(&self) -> *mut dwl_context {
        self.ctx.0
    }

    fn get_surface(&self, surface_id: u32) -> Option<&GpuDisplaySurface> {
        self.surfaces.get(&surface_id)
    }

    /// Imports a dmabuf to the compositor for use as a surface buffer and returns a handle to it.
    pub fn import_dmabuf(
        &mut self,
        fd: RawFd,
        offset: u32,
        stride: u32,
        modifiers: u64,
        width: u32,
        height: u32,
        fourcc: u32,
    ) -> Result<u32, GpuDisplayError> {
        // Safe given that the context pointer is valid. Any other invalid parameters would be
        // rejected by dwl_context_dmabuf_new safely. We check that the resulting dmabuf is valid
        // before filing it away.
        let dmabuf = DwlDmabuf(unsafe {
            dwl_context_dmabuf_new(
                self.ctx(),
                fd,
                offset,
                stride,
                modifiers,
                width,
                height,
                fourcc,
            )
        });
        if dmabuf.0.is_null() {
            return Err(GpuDisplayError::FailedImport);
        }

        let next_id = self.dmabuf_next_id;
        self.dmabufs.insert(next_id, dmabuf);
        self.dmabuf_next_id += 1;
        Ok(next_id)
    }

    /// Releases a previously imported dmabuf identified by the given handle.
    pub fn release_import(&mut self, import_id: u32) {
        self.dmabufs.remove(&import_id);
    }

    /// Dispatches internal events that were received from the compositor since the last call to
    /// `dispatch_events`.
    pub fn dispatch_events(&mut self) {
        // Safe given that the context pointer is valid.
        unsafe {
            dwl_context_dispatch(self.ctx());
        }
    }

    /// Creates a surface on the the compositor as either a top level window, or child of another
    /// surface, returning a handle to the new surface.
    pub fn create_surface(
        &mut self,
        parent_surface_id: Option<u32>,
        width: u32,
        height: u32,
    ) -> Result<u32, GpuDisplayError> {
        let parent_ptr = match parent_surface_id {
            Some(id) => match self.get_surface(id).map(|p| p.surface()) {
                Some(ptr) => ptr,
                None => return Err(GpuDisplayError::InvalidSurfaceId),
            },
            None => null_mut(),
        };
        let row_size = width * BYTES_PER_PIXEL;
        let fb_size = row_size * height;
        let buffer_size = round_up_to_page_size(fb_size as usize * BUFFER_COUNT);
        let mut buffer_shm = SharedMemory::new(Some(
            CStr::from_bytes_with_nul(b"GpuDisplaySurface\0").unwrap(),
        ))
        .map_err(GpuDisplayError::CreateShm)?;
        buffer_shm
            .set_size(buffer_size as u64)
            .map_err(GpuDisplayError::SetSize)?;
        let buffer_mem = MemoryMapping::from_fd(&buffer_shm, buffer_size).unwrap();

        // Safe because only a valid context, parent pointer (if not  None), and buffer FD are used.
        // The returned surface is checked for validity before being filed away.
        let surface = DwlSurface(unsafe {
            dwl_context_surface_new(
                self.ctx(),
                parent_ptr,
                buffer_shm.as_raw_fd(),
                buffer_size,
                fb_size as usize,
                width,
                height,
                row_size,
            )
        });

        if surface.0.is_null() {
            return Err(GpuDisplayError::CreateSurface);
        }

        let next_id = self.surface_next_id;
        self.surfaces.insert(
            next_id,
            GpuDisplaySurface {
                surface,
                buffer_size: fb_size as usize,
                buffer_index: Cell::new(0),
                buffer_mem,
            },
        );

        self.surface_next_id += 1;
        Ok(next_id)
    }

    /// Releases a previously created surface identified by the given handle.
    pub fn release_surface(&mut self, surface_id: u32) {
        self.surfaces.remove(&surface_id);
    }

    /// Gets a reference to an unused framebuffer for the identified surface.
    pub fn framebuffer_memory(&self, surface_id: u32) -> Option<VolatileSlice> {
        let surface = self.get_surface(surface_id)?;
        let buffer_index = (surface.buffer_index.get() + 1) % BUFFER_COUNT;
        surface
            .buffer_mem
            .get_slice(
                (buffer_index * surface.buffer_size) as u64,
                surface.buffer_size as u64,
            )
            .ok()
    }

    /// Commits any pending state for the identified surface.
    pub fn commit(&self, surface_id: u32) {
        match self.get_surface(surface_id) {
            Some(surface) => {
                // Safe because only a valid surface is used.
                unsafe {
                    dwl_surface_commit(surface.surface());
                }
            }
            None => debug_assert!(false, "invalid surface_id {}", surface_id),
        }
    }

    /// Returns true if the next buffer in the buffer queue for the given surface is currently in
    /// use.
    ///
    /// If the next buffer is in use, the memory returned from `framebuffer_memory` should not be
    /// written to.
    pub fn next_buffer_in_use(&self, surface_id: u32) -> bool {
        match self.get_surface(surface_id) {
            Some(surface) => {
                let next_buffer_index = (surface.buffer_index.get() + 1) % BUFFER_COUNT;
                // Safe because only a valid surface and buffer index is used.
                unsafe { dwl_surface_buffer_in_use(surface.surface(), next_buffer_index) }
            }
            None => {
                debug_assert!(false, "invalid surface_id {}", surface_id);
                false
            }
        }
    }

    /// Changes the visible contents of the identified surface to the contents of the framebuffer
    /// last returned by `framebuffer_memory` for this surface.
    pub fn flip(&self, surface_id: u32) {
        match self.get_surface(surface_id) {
            Some(surface) => {
                surface
                    .buffer_index
                    .set((surface.buffer_index.get() + 1) % BUFFER_COUNT);
                // Safe because only a valid surface and buffer index is used.
                unsafe {
                    dwl_surface_flip(surface.surface(), surface.buffer_index.get());
                }
            }
            None => debug_assert!(false, "invalid surface_id {}", surface_id),
        }
    }

    /// Changes the visible contents of the identified surface to that of the identified imported
    /// buffer.
    pub fn flip_to(&self, surface_id: u32, import_id: u32) {
        match self.get_surface(surface_id) {
            Some(surface) => {
                match self.dmabufs.get(&import_id) {
                    // Safe because only a valid surface and dmabuf is used.
                    Some(dmabuf) => unsafe { dwl_surface_flip_to(surface.surface(), dmabuf.0) },
                    None => debug_assert!(false, "invalid import_id {}", import_id),
                }
            }
            None => debug_assert!(false, "invalid surface_id {}", surface_id),
        }
    }

    /// Returns true if the identified top level surface has been told to close by the compositor,
    /// and by extension the user.
    pub fn close_requested(&self, surface_id: u32) -> bool {
        match self.get_surface(surface_id) {
            Some(surface) =>
            // Safe because only a valid surface is used.
            unsafe { dwl_surface_close_requested(surface.surface()) }
            None => false,
        }
    }

    /// Sets the position of the identified subsurface relative to its parent.
    ///
    /// The change in position will not be visible until `commit` is called for the parent surface.
    pub fn set_position(&self, surface_id: u32, x: u32, y: u32) {
        match self.get_surface(surface_id) {
            Some(surface) => {
                // Safe because only a valid surface is used.
                unsafe {
                    dwl_surface_set_position(surface.surface(), x, y);
                }
            }
            None => debug_assert!(false, "invalid surface_id {}", surface_id),
        }
    }
}

impl Drop for GpuDisplay {
    fn drop(&mut self) {
        // Safe given that the context pointer is valid.
        unsafe { dwl_context_destroy(&mut self.ctx.0) }
    }
}

impl AsRawFd for GpuDisplay {
    fn as_raw_fd(&self) -> RawFd {
        // Safe given that the context pointer is valid.
        unsafe { dwl_context_fd(self.ctx.0) }
    }
}
