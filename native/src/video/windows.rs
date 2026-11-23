//! Windows 8+ playback with Media Foundation's media engine in frame-server
//! mode: the engine decodes and plays the sound itself; frames are copied into
//! a WIC bitmap on demand. Windows 7 has no media engine, so `open` fails there
//! and the caller falls back to the system player.

use std::path::Path;
use std::sync::OnceLock;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICBitmap, IWICImagingFactory, WICBitmapCacheOnLoad,
    WICBitmapLockRead, WICRect,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory, IMFMediaEngineNotify,
    IMFMediaEngineNotify_Impl, MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MF_VERSION,
    MFARGB, MFCreateAttributes, MFSTARTUP_FULL, MFStartup, MFVideoNormalizedRect,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::{BSTR, implement};

use super::{Frame, Options, Player};

/// DXGI_FORMAT_B8G8R8A8_UNORM, without pulling in the DXGI bindings.
const BGRA: u32 = 87;

/// The engine requires a callback; state is read back from the engine itself.
#[implement(IMFMediaEngineNotify)]
struct Notify;

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, _event: u32, _param1: usize, _param2: u32) -> windows::core::Result<()> {
        Ok(())
    }
}

fn started() -> Result<(), String> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe {
            // Already initialised by the window (OLE) is fine.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| e.message())
        })
        .clone()
}

struct MfPlayer {
    engine: IMFMediaEngine,
    wic: IWICImagingFactory,
    bitmap: Option<(IWICBitmap, u32, u32)>,
    last_pts: Option<i64>,
    max: (u32, u32),
}

pub fn open(path: &Path, options: Options) -> Result<Box<dyn Player>, String> {
    started()?;
    unsafe {
        let factory: IMFMediaEngineClassFactory =
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("no media engine: {}", e.message()))?;
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 2).map_err(|e| e.message())?;
        let attributes = attributes.ok_or("no attributes")?;
        let notify: IMFMediaEngineNotify = Notify.into();
        attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify).map_err(|e| e.message())?;
        attributes.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, BGRA).map_err(|e| e.message())?;
        let engine = factory.CreateInstance(0, &attributes).map_err(|e| e.message())?;
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(|e| e.message())?;

        engine.SetLoop(options.looped).map_err(|e| e.message())?;
        engine.SetMuted(options.muted).map_err(|e| e.message())?;
        engine.SetAutoPlay(options.autoplay).map_err(|e| e.message())?;
        let url = BSTR::from(path.to_string_lossy().as_ref());
        engine.SetSource(&url).map_err(|e| e.message())?;
        Ok(Box::new(MfPlayer {
            engine,
            wic,
            bitmap: None,
            last_pts: None,
            max: (options.max_width.max(1), options.max_height.max(1)),
        }))
    }
}

/// Largest size with the video's shape that fits `max` (never upscaled).
pub(super) fn fit(width: u32, height: u32, max: (u32, u32)) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let k = (max.0 as f64 / width as f64).min(max.1 as f64 / height as f64).min(1.0);
    (((width as f64 * k).round() as u32).max(1), ((height as f64 * k).round() as u32).max(1))
}

impl MfPlayer {
    fn grab(&mut self) -> Option<Frame> {
        unsafe {
            let (mut w, mut h) = (0u32, 0u32);
            self.engine.GetNativeVideoSize(Some(&mut w), Some(&mut h)).ok()?;
            let (w, h) = fit(w, h, self.max);
            if w == 0 {
                return None;
            }
            if self.bitmap.as_ref().is_none_or(|(_, bw, bh)| (*bw, *bh) != (w, h)) {
                let bitmap = self.wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppBGRA, WICBitmapCacheOnLoad).ok()?;
                self.bitmap = Some((bitmap, w, h));
            }
            let (bitmap, _, _) = self.bitmap.as_ref()?;
            let source = MFVideoNormalizedRect { left: 0.0, top: 0.0, right: 1.0, bottom: 1.0 };
            let target = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            let border = MFARGB { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbAlpha: 255 };
            self.engine.TransferVideoFrame(bitmap, Some(&source), &target, Some(&border)).ok()?;

            let rect = WICRect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
            let lock = bitmap.Lock(&rect, WICBitmapLockRead.0 as u32).ok()?;
            let stride = lock.GetStride().ok()? as usize;
            let mut size = 0u32;
            let mut data = std::ptr::null_mut();
            lock.GetDataPointer(&mut size, &mut data).ok()?;
            if data.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(data, size as usize);
            Some(Frame { width: w, height: h, rgba: bgra_to_rgba(bytes, w as usize, h as usize, stride) })
        }
    }
}

/// Packs BGRA rows of `stride` bytes into tight, opaque RGBA rows.
pub(super) fn bgra_to_rgba(bytes: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let start = row * stride;
        let Some(line) = bytes.get(start..start + width * 4) else { break };
        for px in line.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    out
}

impl Player for MfPlayer {
    fn frame(&mut self) -> Option<Frame> {
        // A new presentation time means a new picture is ready.
        let pts = unsafe { self.engine.OnVideoStreamTick() }.ok()?;
        if self.last_pts == Some(pts) {
            return None;
        }
        let frame = self.grab()?;
        self.last_pts = Some(pts);
        Some(frame)
    }

    fn play(&mut self) {
        unsafe {
            if self.engine.IsEnded().as_bool() {
                let _ = self.engine.SetCurrentTime(0.0);
            }
            let _ = self.engine.Play();
        }
    }

    fn pause(&mut self) {
        let _ = unsafe { self.engine.Pause() };
    }

    fn is_paused(&self) -> bool {
        unsafe { self.engine.IsPaused().as_bool() || self.engine.IsEnded().as_bool() }
    }

    fn seek(&mut self, seconds: f64) {
        let _ = unsafe { self.engine.SetCurrentTime(seconds.max(0.0)) };
        // Show the new spot even while paused.
        self.last_pts = None;
    }

    fn position(&self) -> f64 {
        let t = unsafe { self.engine.GetCurrentTime() };
        if t.is_finite() { t.max(0.0) } else { 0.0 }
    }

    fn duration(&self) -> f64 {
        let d = unsafe { self.engine.GetDuration() };
        if d.is_finite() && d > 0.0 { d } else { 0.0 }
    }

    fn set_volume(&mut self, volume: f32) {
        let _ = unsafe { self.engine.SetVolume(volume.clamp(0.0, 1.0) as f64) };
    }

    fn set_muted(&mut self, muted: bool) {
        let _ = unsafe { self.engine.SetMuted(muted) };
    }

    fn ended(&self) -> bool {
        unsafe { self.engine.IsEnded().as_bool() }
    }

    fn error(&self) -> Option<String> {
        let error = unsafe { self.engine.GetError() }.ok()?;
        let code = unsafe { error.GetErrorCode() };
        (code != 0).then(|| match code {
            1 => "Playback aborted".to_owned(),
            2 => "File read error".to_owned(),
            3 => "Could not decode the video".to_owned(),
            4 => "This video format is not supported by the system".to_owned(),
            _ => format!("Playback error ({code})"),
        })
    }
}

impl Drop for MfPlayer {
    fn drop(&mut self) {
        let _ = unsafe { self.engine.Shutdown() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_without_upscaling() {
        assert_eq!(fit(1920, 1080, (960, 960)), (960, 540));
        assert_eq!(fit(640, 360, (1920, 1080)), (640, 360));
        assert_eq!(fit(0, 10, (100, 100)), (0, 0));
    }

    #[test]
    fn packs_padded_bgra_rows() {
        // 1×2 picture, 8-byte stride (4 bytes of padding per row).
        let bytes = [1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0, 9, 9, 9, 9];
        assert_eq!(bgra_to_rgba(&bytes, 1, 2, 8), vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }
}
