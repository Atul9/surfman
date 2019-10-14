// surfman/src/platform/windows/wgl/context.rs
//
//! Wrapper for WGL contexts on Windows.

use crate::context::CREATE_CONTEXT_MUTEX;
use crate::{ContextAttributeFlags, ContextAttributes, ContextID, Error};
use crate::{GLVersion, WindowingApiError};
use super::device::{Device, HiddenWindow};
use super::surface::{Surface, SurfaceType, Win32Objects};

use crate::gl::types::{GLenum, GLint, GLuint};
use crate::gl::{self, Gl};
use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::mem;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::thread;
use winapi::shared::minwindef::{self, BOOL, FALSE, FLOAT, LPARAM, LPVOID, LRESULT, UINT};
use winapi::shared::minwindef::{WORD, WPARAM};
use winapi::shared::ntdef::{HANDLE, LPCSTR};
use winapi::shared::windef::{HBRUSH, HDC, HGLRC, HWND};
use winapi::um::libloaderapi;
use winapi::um::wingdi::{self, PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_MAIN_PLANE};
use winapi::um::wingdi::{PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA, PIXELFORMATDESCRIPTOR};
use winapi::um::wingdi::{wglCreateContext, wglDeleteContext, wglGetCurrentContext, wglGetCurrentDC, wglGetProcAddress, wglMakeCurrent};
use winapi::um::winuser::{self, COLOR_BACKGROUND, CS_OWNDC, MSG, WM_CREATE, WM_DESTROY};
use winapi::um::winuser::{WNDCLASSA, WS_OVERLAPPEDWINDOW, WS_VISIBLE};

const WGL_DRAW_TO_WINDOW_ARB:        GLenum = 0x2001;
const WGL_ACCELERATION_ARB:          GLenum = 0x2003;
const WGL_SUPPORT_OPENGL_ARB:        GLenum = 0x2010;
const WGL_DOUBLE_BUFFER_ARB:         GLenum = 0x2011;
const WGL_PIXEL_TYPE_ARB:            GLenum = 0x2013;
const WGL_COLOR_BITS_ARB:            GLenum = 0x2014;
const WGL_ALPHA_BITS_ARB:            GLenum = 0x201b;
const WGL_DEPTH_BITS_ARB:            GLenum = 0x2022;
const WGL_STENCIL_BITS_ARB:          GLenum = 0x2023;
const WGL_FULL_ACCELERATION_ARB:     GLenum = 0x2027;
const WGL_TYPE_RGBA_ARB:             GLenum = 0x202b;
const WGL_CONTEXT_MAJOR_VERSION_ARB: GLenum = 0x2091;
const WGL_CONTEXT_MINOR_VERSION_ARB: GLenum = 0x2092;
const WGL_CONTEXT_PROFILE_MASK_ARB:  GLenum = 0x9126;

const WGL_CONTEXT_CORE_PROFILE_BIT_ARB: GLenum = 0x00000001;

#[allow(non_snake_case)]
#[derive(Default)]
pub(crate) struct WGLExtensionFunctions {
    CreateContextAttribsARB: Option<unsafe extern "C" fn(hDC: HDC,
                                                         shareContext: HGLRC,
                                                         attribList: *const c_int)
                                                         -> HGLRC>,
    GetExtensionsStringARB: Option<unsafe extern "C" fn(hdc: HDC) -> *const c_char>,
    pub(crate) pixel_format_functions: Option<WGLPixelFormatExtensionFunctions>,
    pub(crate) dx_interop_functions: Option<WGLDXInteropExtensionFunctions>,
}

#[allow(non_snake_case)]
pub(crate) struct WGLPixelFormatExtensionFunctions {
    ChoosePixelFormatARB: unsafe extern "C" fn(hdc: HDC,
                                               piAttribIList: *const c_int,
                                               pfAttribFList: *const FLOAT,
                                               nMaxFormats: UINT,
                                               piFormats: *mut c_int,
                                               nNumFormats: *mut UINT)
                                               -> BOOL,
    GetPixelFormatAttribivARB: unsafe extern "C" fn(hdc: HDC,
                                                    iPixelFormat: c_int,
                                                    iLayerPlane: c_int,
                                                    nAttributes: UINT,
                                                    piAttributes: *const c_int,
                                                    piValues: *mut c_int)
                                                    -> BOOL,
}

#[allow(non_snake_case)]
pub(crate) struct WGLDXInteropExtensionFunctions {
    pub(crate) DXCloseDeviceNV: unsafe extern "C" fn(hDevice: HANDLE) -> BOOL,
    pub(crate) DXLockObjectsNV: unsafe extern "C" fn(hDevice: HANDLE,
                                                     count: GLint,
                                                     hObjects: *mut HANDLE)
                                                     -> BOOL,
    pub(crate) DXOpenDeviceNV: unsafe extern "C" fn(dxDevice: *mut c_void) -> HANDLE,
    pub(crate) DXRegisterObjectNV: unsafe extern "C" fn(hDevice: HANDLE,
                                                        dxResource: *mut c_void,
                                                        name: GLuint,
                                                        object_type: GLenum,
                                                        access: GLenum)
                                                        -> HANDLE,
    pub(crate) DXSetResourceShareHandleNV: unsafe extern "C" fn(dxResource: *mut c_void,
                                                                shareHandle: HANDLE)
                                                                -> BOOL,
    DXUnlockObjectsNV: unsafe extern "C" fn(hDevice: HANDLE, count: GLint, hObjects: *mut HANDLE)
                                            -> BOOL,
    DXUnregisterObjectNV: unsafe extern "C" fn(hObject: HANDLE) -> BOOL,
}

#[derive(Clone)]
pub struct ContextDescriptor {
    pixel_format: c_int,
    gl_version: GLVersion,
}

pub struct Context {
    pub(crate) id: ContextID,
    glrc: HGLRC,
    pub(crate) gl: Gl,
    hidden_window: Option<HiddenWindow>,
    framebuffer: Framebuffer,
}

lazy_static! {
    pub(crate) static ref WGL_EXTENSION_FUNCTIONS: WGLExtensionFunctions = {
        thread::spawn(extension_loader_thread).join().unwrap()
    };
}

enum Framebuffer {
    None,
    External { dc: HDC },
    Surface(Surface),
}

impl Device {
    #[allow(non_snake_case)]
    pub fn create_context_descriptor(&self, attributes: &ContextAttributes)
                                     -> Result<ContextDescriptor, Error> {
        let flags = attributes.flags;
        let alpha_bits   = if flags.contains(ContextAttributeFlags::ALPHA)   { 8  } else { 0 };
        let depth_bits   = if flags.contains(ContextAttributeFlags::DEPTH)   { 24 } else { 0 };
        let stencil_bits = if flags.contains(ContextAttributeFlags::STENCIL) { 8  } else { 0 };

        let attrib_i_list = [
            WGL_DRAW_TO_WINDOW_ARB as c_int, gl::TRUE as c_int,
            WGL_SUPPORT_OPENGL_ARB as c_int, gl::TRUE as c_int,
            WGL_DOUBLE_BUFFER_ARB as c_int,  gl::TRUE as c_int,
            WGL_PIXEL_TYPE_ARB as c_int,     WGL_TYPE_RGBA_ARB as c_int,
            WGL_ACCELERATION_ARB as c_int,   WGL_FULL_ACCELERATION_ARB as c_int,
            WGL_COLOR_BITS_ARB as c_int,     32,
            WGL_ALPHA_BITS_ARB as c_int,     alpha_bits,
            WGL_DEPTH_BITS_ARB as c_int,     depth_bits,
            WGL_STENCIL_BITS_ARB as c_int,   stencil_bits,
            0,
        ];

        let wglChoosePixelFormatARB = match WGL_EXTENSION_FUNCTIONS.ChoosePixelFormatARB {
            None => return Err(Error::RequiredExtensionUnavailable),
            Some(wglChoosePixelFormatARB) => wglChoosePixelFormatARB,
        };

        let hidden_window_dc = self.hidden_window.get_dc();
        unsafe {
            let (mut pixel_format, mut pixel_format_count) = (0, 0);
            let ok = wglChoosePixelFormatARB(hidden_window_dc.dc,
                                             attrib_i_list.as_ptr(),
                                             ptr::null(),
                                             1,
                                             &mut pixel_format,
                                             &mut pixel_format_count);
            if ok == FALSE {
                return Err(Error::PixelFormatSelectionFailed(WindowingApiError::Failed));
            }
            if pixel_format_count == 0 {
                return Err(Error::NoPixelFormatFound);
            }

            Ok(ContextDescriptor { pixel_format, gl_version: attributes.version })
        }
    }

    pub fn create_context(&mut self, descriptor: &ContextDescriptor, surface_type: &SurfaceType)
                          -> Result<Context, Error> {
        let wglCreateContextAttribsARB = match WGL_EXTENSION_FUNCTIONS.CreateContextAttribsARB {
            None => return Err(Error::RequiredExtensionUnavailable),
            Some(wglCreateContextAttribsARB) => wglCreateContextAttribsARB,
        };

        let mut next_context_id = CREATE_CONTEXT_MUTEX.lock().unwrap();
        unsafe {
            let (glrc, gl);

            // Get a suitable DC.
            let hidden_window = match *surface_type {
                SurfaceType::Widget { ref native_widget } => None,
                SurfaceType::Generic { .. } => Some(HiddenWindow::new()),
            };

            {
                let hidden_window_dc = match hidden_window {
                    None => None,
                    Some(ref hidden_window) => Some(hidden_window.get_dc()),
                };
                let dc = match *surface_type {
                    SurfaceType::Widget { ref native_widget } => {
                        winuser::GetDC(native_widget.window_handle)
                    }
                    SurfaceType::Generic { .. } => hidden_window_dc.as_ref().unwrap().dc,
                };

                // Set the pixel format on the DC.
                let mut pixel_format_descriptor = mem::zeroed();
                let pixel_format_count =
                    wingdi::DescribePixelFormat(dc,
                                                descriptor.pixel_format,
                                                mem::size_of::<PIXELFORMATDESCRIPTOR>() as UINT,
                                                &mut pixel_format_descriptor);
                assert_ne!(pixel_format_count, 0);
                let ok = wingdi::SetPixelFormat(dc,
                                                descriptor.pixel_format,
                                                &mut pixel_format_descriptor);
                assert_ne!(ok, FALSE);

                // Make the context.
                let wgl_attributes = [
                    WGL_CONTEXT_MAJOR_VERSION_ARB as c_int, descriptor.gl_version.major as c_int,
                    WGL_CONTEXT_MINOR_VERSION_ARB as c_int, descriptor.gl_version.minor as c_int,
                    WGL_CONTEXT_PROFILE_MASK_ARB as c_int,
                        WGL_CONTEXT_CORE_PROFILE_BIT_ARB as c_int,
                    0,
                ];
                glrc = wglCreateContextAttribsARB(dc, ptr::null_mut(), wgl_attributes.as_ptr());
                if glrc.is_null() {
                    return Err(Error::ContextCreationFailed(WindowingApiError::Failed));
                }

                // Temporarily make the context current.
                let _guard = CurrentContextGuard::new();
                let ok = wglMakeCurrent(dc, glrc);
                assert_ne!(ok, FALSE);

                // Load the GL functions.
                gl = Gl::load_with(get_proc_address);
            }

            // Create the initial context.
            let mut context = Context {
                id: *next_context_id,
                glrc,
                gl,
                hidden_window,
                framebuffer: Framebuffer::None,
            };
            next_context_id.0 += 1;

            // Build the initial framebuffer.
            let surface = self.create_surface(&context, surface_type)?;
            self.lock_surface(&surface);
            context.framebuffer = Framebuffer::Surface(surface);
            Ok(context)
        }
    }

    pub fn context_descriptor(&self, context: &Context) -> ContextDescriptor {
        unsafe {
            let dc = self.get_context_dc(context);
            let pixel_format = wingdi::GetPixelFormat(dc);

            let _guard = self.temporarily_make_context_current(context);
            let version_string = context.gl.GetString(gl::VERSION);
            let version_string = CStr::from_ptr(version_string).to_string_lossy();
            let mut version_string_iter = version_string.split(".");
            let major_version: u8 =
                version_string_iter.next()
                                   .expect("Where's the major GL version?")
                                   .parse()
                                   .expect("Couldn't parse the major GL version!");
            let minor_version: u8 =
                version_string_iter.next()
                                   .expect("Where's the minor GL version?")
                                   .parse()
                                   .expect("Couldn't parse the minor GL version!");
            ContextDescriptor {
                pixel_format,
                gl_version: GLVersion::new(major_version, minor_version),
            }
        }
    }

    pub fn context_descriptor_attributes(&self, context_descriptor: &ContextDescriptor)
                                         -> ContextAttributes {
        unimplemented!()
    }

    pub fn replace_context_surface(&self, context: &mut Context, new_surface: Surface)
                                   -> Result<Surface, Error> {
        if let Framebuffer::External = context.framebuffer {
            return Err(Error::ExternalRenderTarget)
        }

        if context.id != new_surface.context_id {
            return Err(Error::IncompatibleSurface);
        }

        unsafe {
            let is_current = self.context_is_current(context);

            let old_surface = self.release_surface(context).expect("Where's our surface?");
            self.attach_surface(context, new_surface);

            if is_current {
                // We need to make ourselves current again, because the surface changed.
                self.make_context_current(context)?;
            }

            Ok(old_surface)
        }
    }

    pub(crate) fn temporarily_bind_framebuffer(&self, framebuffer: GLuint) {
        unimplemented!()
    }

    pub(crate) fn temporarily_make_context_current(&self, context: &Context)
                                                   -> Result<(), Error> {
        unimplemented!()
    }

    fn attach_surface(&self, context: &mut Context, surface: Surface) {
        match context.framebuffer {
            Framebuffer::None => context.framebuffer = Framebuffer::Surface(surface),
            _ => panic!("Tried to attach a surface, but there was already a surface present!"),
        }
    }

    fn release_surface(&self, context: &mut Context) -> Option<Surface> {
        match mem::replace(&mut context.framebuffer, Framebuffer::None) {
            Framebuffer::Surface(surface) => Some(surface),
            Framebuffer::None | Framebuffer::External => None,
        }
    }

    fn get_context_dc<'a>(&self, context: &'a Context) -> DCGuard<'a> {
        unsafe {
            match context.framebuffer {
                Framebuffer::None => unreachable!(),
                Framebuffer::External { dc } => DCGuard::new(dc, None),
                Framebuffer::Surface(ref surface) => {
                    match surface.win32_objects {
                        Win32Objects::Window { window } => {
                            DCGuard::new(winuser::GetDC(window), Some(window))
                        }
                        Win32Objects::Texture { .. } => {
                            context.hidden_window.as_ref().unwrap().get_dc()
                        }
                    }
                }
            };
        }
    }
}

fn extension_loader_thread() -> WGLExtensionFunctions {
    unsafe {
        let instance = libloaderapi::GetModuleHandleA(ptr::null_mut());
        let window_class_name = &b"SurfmanFalseWindow\0"[0] as *const u8 as LPCSTR;
        let window_class = WNDCLASSA {
            style: CS_OWNDC,
            lpfnWndProc: Some(extension_loader_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: ptr::null_mut(),
            hCursor: ptr::null_mut(),
            hbrBackground: COLOR_BACKGROUND as HBRUSH,
            lpszMenuName: ptr::null_mut(),
            lpszClassName: window_class_name,
        };
        let window_class_atom = winuser::RegisterClassA(&window_class);
        assert_ne!(window_class_atom, 0);

        let mut extension_functions = WGLExtensionFunctions::default();
        let window = winuser::CreateWindowExA(
            0,
            window_class_atom as LPCSTR,
            window_class_name,
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            0,
            0,
            640,
            480,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            &mut extension_functions as *mut WGLExtensionFunctions as LPVOID);

        let mut msg: MSG = mem::zeroed();
        while winuser::GetMessageA(&mut msg, window, 0, 0) != FALSE {
            winuser::TranslateMessage(&msg);
            winuser::DispatchMessageA(&msg);
            if minwindef::LOWORD(msg.message) as UINT == WM_DESTROY {
                break;
            }
        }

        extension_functions
    }
}

#[allow(non_snake_case)]
extern "system" fn extension_loader_window_proc(hwnd: HWND,
                                                uMsg: UINT,
                                                wParam: WPARAM,
                                                lParam: LPARAM)
                                                -> LRESULT {
    unsafe {
        match uMsg {
            WM_CREATE => {
                let pixel_format_descriptor = PIXELFORMATDESCRIPTOR {
                    nSize: mem::size_of::<PIXELFORMATDESCRIPTOR>() as WORD,
                    nVersion: 1,
                    dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                    iPixelType: PFD_TYPE_RGBA,
                    cColorBits: 32,
                    cRedBits: 0,
                    cRedShift: 0,
                    cGreenBits: 0,
                    cGreenShift: 0,
                    cBlueBits: 0,
                    cBlueShift: 0,
                    cAlphaBits: 0,
                    cAlphaShift: 0,
                    cAccumBits: 0,
                    cAccumRedBits: 0,
                    cAccumGreenBits: 0,
                    cAccumBlueBits: 0,
                    cAccumAlphaBits: 0,
                    cDepthBits: 24,
                    cStencilBits: 8,
                    cAuxBuffers: 0,
                    iLayerType: PFD_MAIN_PLANE,
                    bReserved: 0,
                    dwLayerMask: 0,
                    dwVisibleMask: 0,
                    dwDamageMask: 0,
                };

                // Create a false GL context.
                let dc = winuser::GetDC(hwnd);
                let pixel_format = wingdi::ChoosePixelFormat(dc, &pixel_format_descriptor);
                assert_ne!(pixel_format, 0);
                let mut ok = wingdi::SetPixelFormat(dc, pixel_format, &pixel_format_descriptor);
                assert_ne!(ok, FALSE);
                let gl_context = wglCreateContext(dc);
                assert!(!gl_context.is_null());
                ok = wglMakeCurrent(dc, gl_context);
                assert_ne!(ok, FALSE);

                // Detect extensions.
                let wgl_extension_functions = lParam as *mut WGLExtensionFunctions;
                (*wgl_extension_functions).GetExtensionsStringARB = mem::transmute(
                    wglGetProcAddress(&b"wglGetExtensionsStringARB\0"[0] as *const u8 as LPCSTR));
                let extensions = match (*wgl_extension_functions).GetExtensionsStringARB {
                    Some(wglGetExtensionsStringARB) => {
                        CStr::from_ptr(wglGetExtensionsStringARB(dc)).to_string_lossy()
                    }
                    None => Cow::Borrowed(""),
                };

                // Load function pointers.
                for extension in extensions.split(' ') {
                    if extension == "WGL_ARB_pixel_format" {
                        (*wgl_extension_functions).pixel_format_functions =
                            Some(WGLPixelFormatExtensionFunctions {
                                ChoosePixelFormatARB: mem::transmute(wglGetProcAddress(
                                    &b"wglChoosePixelFormatARB\0"[0] as *const u8 as LPCSTR)),
                                GetPixelFormatAttribivARB: mem::transmute(wglGetProcAddress(
                                    &b"wglGetPixelFormatAttribivARB\0"[0] as *const u8 as LPCSTR)),
                            });
                        continue;
                    }
                    if extension == "WGL_ARB_create_context" {
                        (*wgl_extension_functions).CreateContextAttribsARB = mem::transmute(
                            wglGetProcAddress(&b"wglCreateContextAttribsARB\0"[0] as *const u8 as
                            LPCSTR));
                        continue;
                    }
                    if extension == "WGL_NV_DX_interop" {
                        (*wgl_extension_functions).dx_interop_functions =
                            Some(WGLDXInteropExtensionFunctions {
                                DXCloseDeviceNV: mem::transmute(wglGetProcAddress(
                                    &b"wglDXCloseDeviceNV\0"[0] as *const u8 as LPCSTR)),
                                DXLockObjectsNV: mem::transmute(wglGetProcAddress(
                                    &b"wglDXLockObjectsNV\0"[0] as *const u8 as LPCSTR)),
                                DXOpenDeviceNV: mem::transmute(wglGetProcAddress(
                                    &b"wglDXOpenDeviceNV\0"[0] as *const u8 as LPCSTR)),
                                DXRegisterObjectNV: mem::transmute(wglGetProcAddress(
                                    &b"wglDXRegisterObjectNV\0"[0] as *const u8 as LPCSTR)),
                                DXSetResourceShareHandleNV: mem::transmute(wglGetProcAddress(
                                    &b"wglDXSetResourceShareHandleNV\0"[0] as *const u8 as
                                    LPCSTR)),
                                DXUnlockObjectsNV: mem::transmute(wglGetProcAddress(
                                    &b"wglDXUnlockObjectsNV\0"[0] as *const u8 as LPCSTR)),
                                DXUnregisterObjectNV: mem::transmute(wglGetProcAddress(
                                    &b"wglDXUnregisterObjectNV\0"[0] as *const u8 as LPCSTR)),
                            });
                        continue;
                    }
                }

                wglDeleteContext(gl_context);
                winuser::DestroyWindow(hwnd);
                0
            }
            _ => winuser::DefWindowProcA(hwnd, uMsg, wParam, lParam),
        }
    }
}

#[must_use]
struct CurrentContextGuard {
    old_dc: HDC,
    old_glrc: HGLRC,
}

impl Drop for CurrentContextGuard {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            wglMakeCurrent(self.old_dc, self.old_glrc);
        }
    }
}

impl CurrentContextGuard {
    #[inline]
    fn new() -> CurrentContextGuard {
        unsafe {
            CurrentContextGuard {
                old_dc: wglGetCurrentDC(),
                old_glrc: wglGetCurrentContext(),
            }
        }
    }
}

fn get_proc_address(symbol_name: &str) -> *const c_void {
    unsafe {
        let symbol_name: CString = CString::new(symbol_name).unwrap();
        wglGetProcAddress(symbol_name.as_ptr() as *const u8 as LPCSTR) as *const c_void
    }
}
