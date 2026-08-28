//! System audio via WASAPI loopback on the default render endpoint.
//!
//! Runs a capture thread that pulls packets from `IAudioCaptureClient` and converts them to
//! interleaved `f32` (the shared-mode mix format is normally 32-bit float; 16-bit PCM is
//! handled too). Only verified by CI/real hardware — keep the WASAPI call sequence textbook.

use super::{AudioFormat, AudioSource};
use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// 100 ns units: 200 ms shared-mode buffer.
const BUFFER_DURATION_HNS: i64 = 2_000_000;

#[derive(Clone, Copy)]
enum Sample {
    F32,
    I16,
}

struct WasapiAudio {
    format: AudioFormat,
    rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSource for WasapiAudio {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<f32>>> {
        if let Some(e) = self.error.lock().ok().and_then(|e| e.clone()) {
            bail!("{e}");
        }
        match self.rx.recv_timeout(timeout) {
            Ok(pcm) => Ok(Some(pcm)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => bail!("audio thread ended"),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WasapiAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: AudioFormat,
    sample: Sample,
    channels: usize,
}

/// Open the loopback client on the calling (COM-initialised) thread.
fn open() -> Result<Opened> {
    // SAFETY: standard COM start-up; RPC_E_CHANGED_MODE is fine.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() && hr.0 != 0x8001_0106_u32 as i32 {
            bail!("CoInitializeEx failed: {hr}");
        }
    }
    // SAFETY: documented WASAPI initialisation sequence.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("MMDeviceEnumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("default render endpoint")?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("IAudioClient")?;
        let fmt_ptr = client.GetMixFormat().context("GetMixFormat")?;
        if fmt_ptr.is_null() {
            bail!("GetMixFormat returned null");
        }
        // WAVEFORMATEX is packed: copy fields out before using them.
        let fmt: WAVEFORMATEX = std::ptr::read_unaligned(fmt_ptr);
        let channels = { fmt.nChannels } as usize;
        let sample_rate = { fmt.nSamplesPerSec };
        let tag = { fmt.wFormatTag } as u32;
        let bits = { fmt.wBitsPerSample };
        let sample = if tag == WAVE_FORMAT_IEEE_FLOAT {
            Sample::F32
        } else if tag == WAVE_FORMAT_PCM && bits == 16 {
            Sample::I16
        } else if tag == WAVE_FORMAT_EXTENSIBLE {
            let ext: WAVEFORMATEXTENSIBLE =
                std::ptr::read_unaligned(fmt_ptr as *const WAVEFORMATEXTENSIBLE);
            let sub = { ext.SubFormat };
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                Sample::F32
            } else if bits == 16 {
                Sample::I16
            } else {
                CoTaskMemFree(Some(fmt_ptr as *const _));
                bail!("unsupported mix format ({bits} bit)");
            }
        } else {
            CoTaskMemFree(Some(fmt_ptr as *const _));
            bail!("unsupported mix format tag {tag}");
        };
        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            BUFFER_DURATION_HNS,
            0,
            fmt_ptr,
            None,
        );
        CoTaskMemFree(Some(fmt_ptr as *const _));
        init.context("IAudioClient::Initialize (loopback)")?;
        let capture: IAudioCaptureClient = client.GetService().context("IAudioCaptureClient")?;
        client.Start().context("IAudioClient::Start")?;
        Ok(Opened {
            client,
            capture,
            format: AudioFormat {
                sample_rate,
                channels: channels as u16,
            },
            sample,
            channels,
        })
    }
}

fn capture_loop(
    opened: Opened,
    tx: Sender<Vec<f32>>,
    rx_keep: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let Opened {
        client,
        capture,
        sample,
        channels,
        ..
    } = opened;
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: documented capture sequence; buffers are valid until ReleaseBuffer.
        unsafe {
            let mut packet = capture.GetNextPacketSize().context("GetNextPacketSize")?;
            if packet == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            while packet > 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .context("GetBuffer")?;
                let n = frames as usize * channels;
                let pcm: Vec<f32> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0
                    || data.is_null()
                {
                    vec![0.0; n]
                } else {
                    match sample {
                        Sample::F32 => std::slice::from_raw_parts(data as *const f32, n).to_vec(),
                        Sample::I16 => std::slice::from_raw_parts(data as *const i16, n)
                            .iter()
                            .map(|&s| s as f32 / 32768.0)
                            .collect(),
                    }
                };
                capture.ReleaseBuffer(frames).context("ReleaseBuffer")?;
                match tx.try_send(pcm) {
                    Ok(()) => {}
                    Err(TrySendError::Full(pcm)) => {
                        let _ = rx_keep.try_recv();
                        let _ = tx.try_send(pcm);
                    }
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                }
                packet = capture.GetNextPacketSize().context("GetNextPacketSize")?;
            }
        }
    }
    // SAFETY: stopping a started client.
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

pub fn create() -> Result<Box<dyn AudioSource>> {
    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(32);
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<AudioFormat, String>>(1);
    let stop = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None::<String>));
    let thread = {
        let stop = Arc::clone(&stop);
        let error = Arc::clone(&error);
        let rx_keep = rx.clone();
        std::thread::Builder::new()
            .name("audio-loopback".into())
            .spawn(move || {
                let opened = match open() {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(opened.format));
                if let Err(e) = capture_loop(opened, tx, rx_keep, stop) {
                    if let Ok(mut err) = error.lock() {
                        err.get_or_insert(format!("{e:#}"));
                    }
                }
            })
            .context("spawning audio thread")?
    };
    let format = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(f)) => f,
        Ok(Err(m)) => bail!("{m}"),
        Err(_) => bail!("timed out opening WASAPI loopback"),
    };
    tracing::info!(?format, "WASAPI loopback capture started");
    Ok(Box::new(WasapiAudio {
        format,
        rx,
        stop,
        error,
        thread: Some(thread),
    }))
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<WasapiAudio>();
    let _ = anyhow!("unused");
}
