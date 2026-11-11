//! Native screen recorder (`ipc record`). Each rendered frame is converted to
//! NV12 on the GPU, straight into dmabufs a V4L2 stateful H.264 encoder
//! imports; no pixel passes through the CPU. Output is a raw Annex-B stream at
//! PATH plus `PATH.ts`, one `TIME_NS DROPPED_BEFORE` line per coded frame.
//! Muxing is `scripts/record.sh`'s job.

#[cfg(not(all(target_pointer_width = "64", target_endian = "little")))]
compile_error!("the V4L2 structs here are laid out for 64-bit little-endian");

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::DrmDeviceFd;
use tracing::{info, warn};

const fn fourcc(c: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*c)
}
const PIX_H264: u32 = fourcc(b"H264");
const PIX_NV12M: u32 = fourcc(b"NM12");

const OUTPUT: u32 = 10; // VIDEO_OUTPUT_MPLANE: raw frames in
const CAPTURE: u32 = 9; // VIDEO_CAPTURE_MPLANE: bitstream out
const MEMORY_MMAP: u32 = 1;
const MEMORY_DMABUF: u32 = 4;
const CAP_M2M_MPLANE: u32 = 0x4000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_FLAG_LAST: u32 = 0x0010_0000;
const ENC_CMD_STOP: u32 = 1;
const SEL_TGT_CROP: u32 = 0;

const CID_B_FRAMES: u32 = 0x0099_09ca;
const CID_GOP_SIZE: u32 = 0x0099_09cb;
const CID_BITRATE: u32 = 0x0099_09cf;
const CID_FRAME_RC: u32 = 0x0099_09d7;
const CID_HEADER_MODE: u32 = 0x0099_09d8;
const CID_H264_8X8: u32 = 0x0099_0a63;
const CID_H264_ENTROPY: u32 = 0x0099_0a65;
const CID_H264_LEVEL: u32 = 0x0099_0a67;
const CID_H264_PROFILE: u32 = 0x0099_0a6b;

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct FmtDesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PlaneFmt {
    sizeimage: u32,
    bytesperline: u32,
    reserved: [u16; 6],
}

/// `v4l2_format` with the `pix_mp` arm of its union.
#[repr(C)]
struct Format {
    type_: u32,
    _align: u32,
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    colorspace: u32,
    plane_fmt: [PlaneFmt; 8],
    num_planes: u8,
    flags: u8,
    ycbcr_enc: u8,
    quantization: u8,
    xfer_func: u8,
    reserved: [u8; 7],
    _rest: [u8; 8],
}

#[repr(C)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Plane {
    bytesused: u32,
    length: u32,
    /// Union of `mem_offset` and `fd`; both sit in the low half (little-endian).
    m: u64,
    data_offset: u32,
    reserved: [u32; 11],
}

#[repr(C)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: [u8; 16],
    sequence: u32,
    memory: u32,
    planes: *mut Plane,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

#[repr(C)]
struct Control {
    id: u32,
    value: i32,
}

/// `v4l2_streamparm` with the `output` arm of its union.
#[repr(C)]
struct StreamParm {
    type_: u32,
    capability: u32,
    outputmode: u32,
    numerator: u32,
    denominator: u32,
    _rest: [u32; 46],
}

#[repr(C)]
struct EncoderCmd {
    cmd: u32,
    flags: u32,
    raw: [u32; 8],
}

#[repr(C)]
struct Selection {
    type_: u32,
    target: u32,
    flags: u32,
    rect: [u32; 4],
    reserved: [u32; 9],
}

const READ: u32 = 2;
const WRITE: u32 = 1;
const fn ioc<T>(dir: u32, nr: u32) -> u32 {
    dir << 30 | (size_of::<T>() as u32) << 16 | (b'V' as u32) << 8 | nr
}
const QUERYCAP: u32 = ioc::<Capability>(READ, 0);
const ENUM_FMT: u32 = ioc::<FmtDesc>(READ | WRITE, 2);
const S_FMT: u32 = ioc::<Format>(READ | WRITE, 5);
const REQBUFS: u32 = ioc::<RequestBuffers>(READ | WRITE, 8);
const QUERYBUF: u32 = ioc::<Buffer>(READ | WRITE, 9);
const QBUF: u32 = ioc::<Buffer>(READ | WRITE, 15);
const DQBUF: u32 = ioc::<Buffer>(READ | WRITE, 17);
const STREAMON: u32 = ioc::<u32>(WRITE, 18);
const STREAMOFF: u32 = ioc::<u32>(WRITE, 19);
const S_PARM: u32 = ioc::<StreamParm>(READ | WRITE, 22);
const S_CTRL: u32 = ioc::<Control>(READ | WRITE, 28);
const ENCODER_CMD: u32 = ioc::<EncoderCmd>(READ | WRITE, 77);
const S_SELECTION: u32 = ioc::<Selection>(READ | WRITE, 95);

fn ioctl<T>(fd: RawFd, req: u32, arg: &mut T) -> io::Result<()> {
    // SAFETY: every request constant above encodes the size of the `T` it is
    // used with, and `arg` is a live, exclusive `T`.
    if unsafe { libc::ioctl(fd, req as _, arg as *mut T) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn zeroed<T>() -> T {
    // SAFETY: only used for the plain-integer ioctl structs above (a null
    // `planes` pointer included).
    unsafe { std::mem::zeroed() }
}

fn has_format(fd: RawFd, type_: u32, want: u32) -> bool {
    (0..)
        .map_while(|index| {
            let mut d: FmtDesc = zeroed();
            (d.index, d.type_) = (index, type_);
            ioctl(fd, ENUM_FMT, &mut d).ok().map(|()| d.pixelformat)
        })
        .any(|f| f == want)
}

/// The first multiplanar M2M node that turns NV12M into H.264.
fn find_encoder() -> Option<(File, String)> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("video"))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().find_map(|path| {
        let file = File::options()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        let fd = file.as_raw_fd();
        let mut cap: Capability = zeroed();
        ioctl(fd, QUERYCAP, &mut cap).ok()?;
        let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
            cap.device_caps
        } else {
            cap.capabilities
        };
        (caps & CAP_M2M_MPLANE != 0
            && has_format(fd, CAPTURE, PIX_H264)
            && has_format(fd, OUTPUT, PIX_NV12M))
        .then(|| (file, path.display().to_string()))
    })
}

/// Encoded size for an output: even, and the width a multiple of 64.
// ponytail: gbm's stride must equal the encoder's (16-aligned on s5p-mfc);
// only 64-aligned widths were tried. `start` refuses a mismatch.
pub fn record_size(output: (i32, i32), scale: f64) -> (u32, u32) {
    let scale = if scale.is_finite() {
        scale.clamp(0.1, 1.0)
    } else {
        1.0
    };
    let w = (f64::from(output.0) * scale).round() as u32;
    let h = (f64::from(output.1) * scale).round() as u32;
    ((w & !63).max(64), (h & !1).max(64))
}

/// Whether an Annex-B chunk carries a coded slice, not just parameter sets.
fn has_slice(data: &[u8]) -> bool {
    data.windows(4)
        .any(|w| w[..3] == [0, 0, 1] && matches!(w[3] & 0x1f, 1 | 5))
}

fn timeval(ns: u64) -> libc::timeval {
    libc::timeval {
        tv_sec: (ns / 1_000_000_000) as _,
        tv_usec: (ns % 1_000_000_000 / 1000) as _,
    }
}

fn micros(t: &libc::timeval) -> i64 {
    t.tv_sec * 1_000_000 + t.tv_usec
}

fn set_ctrl(fd: RawFd, id: u32, value: i32) -> bool {
    ioctl(fd, S_CTRL, &mut Control { id, value }).is_ok()
}

/// The OUTPUT queue's plane layout, as the driver settled it.
struct Layout {
    stride: [u32; 2],
    size: [u32; 2],
}

fn configure(fd: RawFd, (w, h): (u32, u32), fps: u32) -> Result<Layout, String> {
    let mut f: Format = zeroed();
    (f.type_, f.width, f.height) = (CAPTURE, w, h);
    (f.pixelformat, f.num_planes) = (PIX_H264, 1);
    ioctl(fd, S_FMT, &mut f).map_err(|e| format!("S_FMT h264: {e}"))?;

    let mut f: Format = zeroed();
    (f.type_, f.width, f.height) = (OUTPUT, w, h);
    (f.pixelformat, f.num_planes) = (PIX_NV12M, 2);
    // BT.709, limited range: what `capture::NV12_SHADER` writes.
    (f.colorspace, f.xfer_func, f.ycbcr_enc, f.quantization) = (3, 1, 3, 2);
    ioctl(fd, S_FMT, &mut f).map_err(|e| format!("S_FMT nv12m: {e}"))?;
    if f.pixelformat != PIX_NV12M || f.num_planes != 2 {
        return Err("encoder refused NV12M".into());
    }
    let layout = Layout {
        stride: [f.plane_fmt[0].bytesperline, f.plane_fmt[1].bytesperline],
        size: [f.plane_fmt[0].sizeimage, f.plane_fmt[1].sizeimage],
    };

    // S_FMT may have padded the size; the crop is the visible frame. Drivers
    // that take the S_FMT size as visible may refuse this.
    let mut s: Selection = zeroed();
    (s.type_, s.target, s.rect) = (OUTPUT, SEL_TGT_CROP, [0, 0, w, h]);
    let _ = ioctl(fd, S_SELECTION, &mut s);

    let mut p: StreamParm = zeroed();
    (p.type_, p.numerator, p.denominator) = (OUTPUT, 1, fps);
    let _ = ioctl(fd, S_PARM, &mut p);

    // Best effort: each encoder has its own subset.
    set_ctrl(fd, CID_B_FRAMES, 0);
    set_ctrl(fd, CID_HEADER_MODE, 1); // joined with the first frame
    set_ctrl(fd, CID_GOP_SIZE, fps as i32);
    set_ctrl(fd, CID_FRAME_RC, 1);
    set_ctrl(fd, CID_BITRATE, (w * h * 8).min(i32::MAX as u32) as i32);
    set_ctrl(fd, CID_H264_PROFILE, 4); // High
    set_ctrl(fd, CID_H264_ENTROPY, 1); // CABAC
    set_ctrl(fd, CID_H264_8X8, 1);
    // 6.2, 5.2, 5.1, 4.2, 4: the highest the driver's menu has.
    let _ = [19, 16, 15, 13, 11]
        .into_iter()
        .any(|l| set_ctrl(fd, CID_H264_LEVEL, l));
    Ok(layout)
}

fn reqbufs(fd: RawFd, type_: u32, memory: u32, count: u32) -> Result<u32, String> {
    let mut r: RequestBuffers = zeroed();
    (r.count, r.type_, r.memory) = (count, type_, memory);
    ioctl(fd, REQBUFS, &mut r).map_err(|e| format!("REQBUFS type {type_}: {e}"))?;
    Ok(r.count)
}

fn buffer(type_: u32, memory: u32, index: u32, planes: &mut [Plane]) -> Buffer {
    let mut b: Buffer = zeroed();
    (b.type_, b.memory, b.index) = (type_, memory, index);
    (b.planes, b.length) = (planes.as_mut_ptr(), planes.len() as u32);
    b
}

/// A mapped bitstream buffer.
struct Mapped(*mut libc::c_void, usize);
// SAFETY: a private mapping handle, only touched by the drain thread.
unsafe impl Send for Mapped {}
impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: the mapping `map_capture` made, unmapped once.
        unsafe { libc::munmap(self.0, self.1) };
    }
}

fn map_capture(fd: RawFd, count: u32) -> Result<Vec<Mapped>, String> {
    (0..count)
        .map(|i| {
            let mut planes = [zeroed::<Plane>()];
            let mut b = buffer(CAPTURE, MEMORY_MMAP, i, &mut planes);
            ioctl(fd, QUERYBUF, &mut b).map_err(|e| format!("QUERYBUF: {e}"))?;
            let len = planes[0].length as usize;
            // SAFETY: a fresh shared mapping of the driver's buffer; `Mapped`
            // owns it.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    planes[0].m as libc::off_t,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(format!("mmap: {}", io::Error::last_os_error()));
            }
            let mapped = Mapped(ptr, len);
            ioctl(fd, QBUF, &mut b).map_err(|e| format!("QBUF capture: {e}"))?;
            Ok(mapped)
        })
        .collect()
}

fn alloc(gbm: &GbmDevice<DrmDeviceFd>, w: u32, h: u32, format: Fourcc) -> Result<Dmabuf, String> {
    let flags = GbmBufferFlags::RENDERING | GbmBufferFlags::LINEAR;
    let bo = gbm
        .create_buffer_object::<()>(w, h, format, flags)
        .map_err(|e| format!("gbm {format:?} {w}x{h}: {e}"))?;
    GbmBuffer::from_bo(bo, false)
        .export()
        .map_err(|e| format!("dmabuf export: {e}"))
}

struct Job {
    index: u32,
    ts_ns: u64,
    dropped: u64,
    /// Signals when the GPU has written the pair.
    fence: OwnedFd,
}

struct Shared {
    dev: File,
    /// Bit per buffer pair the encoder isn't holding.
    free: AtomicU32,
    dropped: AtomicU64,
    /// Frames in the encoder, oldest first: µs key, exact ns, drops before it.
    queued: Mutex<VecDeque<(i64, u64, u64)>>,
    /// CAPTURE is streaming; without it the encoder never sends LAST.
    capturing: AtomicBool,
    draining: AtomicBool,
}

pub struct Recorder {
    /// Y (R8) and CbCr (GR88) targets, sized like the encoder's planes.
    pairs: Vec<(Dmabuf, Dmabuf)>,
    pub size: (u32, u32),
    pub device: String,
    shared: Arc<Shared>,
    jobs: Option<Sender<Job>>,
    drain: Option<JoinHandle<u64>>,
}

impl Recorder {
    pub fn start(
        gbm: &GbmDevice<DrmDeviceFd>,
        path: &str,
        size: (u32, u32),
        fps: u32,
    ) -> Result<Self, String> {
        let (dev, device) = find_encoder().ok_or("no NV12M to H.264 encoder in /dev/video*")?;
        let fd = dev.as_raw_fd();
        let layout = configure(fd, size, fps.max(1))?;

        let count = reqbufs(fd, OUTPUT, MEMORY_DMABUF, 8)?.clamp(1, 32);
        let pairs = (0..count)
            .map(|_| {
                // Whole rows of the encoder's plane, padding included.
                let rows = |i: usize| layout.size[i].div_ceil(layout.stride[i]);
                let y = alloc(gbm, size.0, rows(0), Fourcc::R8)?;
                let c = alloc(gbm, size.0 / 2, rows(1), Fourcc::Gr88)?;
                for (buf, want) in [(&y, layout.stride[0]), (&c, layout.stride[1])] {
                    let got = buf.strides().next().unwrap_or(0);
                    if got != want {
                        return Err(format!("stride {got}, encoder wants {want}"));
                    }
                }
                Ok((y, c))
            })
            .collect::<Result<Vec<_>, String>>()?;

        let n = reqbufs(fd, CAPTURE, MEMORY_MMAP, 4)?;
        let mapped = map_capture(fd, n)?;
        ioctl(fd, STREAMON, &mut { OUTPUT }).map_err(|e| format!("STREAMON: {e}"))?;

        let create = |p: String| File::create(&p).map_err(|e| format!("{p}: {e}"));
        let stream = create(path.to_string())?;
        let ts = BufWriter::new(create(format!("{path}.ts"))?);

        let shared = Arc::new(Shared {
            dev,
            free: AtomicU32::new(u32::MAX >> (32 - count)),
            dropped: AtomicU64::new(0),
            queued: Mutex::new(VecDeque::new()),
            capturing: AtomicBool::new(false),
            draining: AtomicBool::new(false),
        });
        let (jobs, rx) = channel();
        let (s, bufs, sizes) = (shared.clone(), pairs.clone(), layout.size);
        let thread = |name: &str| std::thread::Builder::new().name(name.into());
        thread("record-submit")
            .spawn(move || submit_loop(&s, rx, &bufs, sizes))
            .map_err(|e| format!("spawn: {e}"))?;
        let s = shared.clone();
        let drain = thread("record-drain")
            .spawn(move || drain_loop(&s, mapped, stream, ts))
            .map_err(|e| format!("spawn: {e}"))?;
        info!(
            device,
            w = size.0,
            h = size.1,
            pairs = count,
            "recording to {path}"
        );
        Ok(Self {
            pairs,
            size,
            device,
            shared,
            jobs: Some(jobs),
            drain: Some(drain),
        })
    }

    /// A pair to draw into, or `None` (counted as a drop) while the encoder
    /// holds them all. Never blocks.
    pub fn acquire(&mut self) -> Option<(u32, &mut Dmabuf, &mut Dmabuf)> {
        let free = self.shared.free.load(Ordering::Acquire);
        if free == 0 {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let index = free.trailing_zeros();
        self.shared.free.fetch_and(!(1 << index), Ordering::AcqRel);
        let (y, c) = &mut self.pairs[index as usize];
        Some((index, y, c))
    }

    /// Hands back a pair whose draw failed.
    pub fn abandon(&mut self, index: u32) {
        self.shared.free.fetch_or(1 << index, Ordering::AcqRel);
        self.shared.dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn submit(&mut self, index: u32, ts_ns: u64, fence: OwnedFd) {
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job {
                index,
                ts_ns,
                dropped,
                fence,
            });
        }
    }

    /// Drains the encoder and closes the files. `frames=N dropped=M`.
    pub fn finish(mut self) -> String {
        let frames = self.stop();
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        format!("frames={frames} dropped={dropped}")
    }

    fn stop(&mut self) -> u64 {
        // Closing the channel makes the submit thread send the drain command.
        self.jobs = None;
        self.drain
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn poll(fd: RawFd, events: libc::c_short, timeout_ms: i32) -> libc::c_short {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one live pollfd.
    unsafe { libc::poll(&mut p, 1, timeout_ms) };
    p.revents
}

/// Queues each frame once its fence signals; the encoder honours no implicit
/// dma-buf fence.
fn submit_loop(shared: &Shared, jobs: Receiver<Job>, pairs: &[(Dmabuf, Dmabuf)], sizes: [u32; 2]) {
    let fd = shared.dev.as_raw_fd();
    // s5p-mfc refuses to start CAPTURE (ENOBUFS) until it has seen two
    // dmabuf frames on OUTPUT, so the bitstream side starts late.
    let (mut sent, mut capturing) = (0, false);
    for job in jobs {
        poll(job.fence.as_raw_fd(), libc::POLLIN, 1000);
        let (y, c) = &pairs[job.index as usize];
        let mut planes = [zeroed::<Plane>(); 2];
        for (plane, buf, size) in [(0, y, sizes[0]), (1, c, sizes[1])] {
            let Some(handle) = buf.handles().next() else {
                continue;
            };
            planes[plane].m = handle.as_fd().as_raw_fd() as u32 as u64;
            // vb2 checks `bytesused` against this, not the dmabuf.
            (planes[plane].bytesused, planes[plane].length) = (size, size);
        }
        let mut b = buffer(OUTPUT, MEMORY_DMABUF, job.index, &mut planes);
        b.timestamp = timeval(job.ts_ns);
        let key = micros(&b.timestamp);
        shared
            .queued
            .lock()
            .unwrap()
            .push_back((key, job.ts_ns, job.dropped));
        if let Err(e) = ioctl(fd, QBUF, &mut b) {
            warn!("record: QBUF frame failed: {e}");
            shared.queued.lock().unwrap().pop_back();
            shared.free.fetch_or(1 << job.index, Ordering::AcqRel);
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        sent += 1;
        if !capturing && sent >= 2 {
            capturing = ioctl(fd, STREAMON, &mut { CAPTURE }).is_ok();
            shared.capturing.store(capturing, Ordering::Release);
        }
    }
    if !capturing {
        warn!("record: under two frames, nothing encoded");
    }
    let mut cmd: EncoderCmd = zeroed();
    cmd.cmd = ENC_CMD_STOP;
    if let Err(e) = ioctl(fd, ENCODER_CMD, &mut cmd) {
        warn!("record: ENC_CMD_STOP failed: {e}");
    }
    shared.draining.store(true, Ordering::Release);
}

/// Writes the bitstream and timestamps, and frees returned pairs. Ends at the
/// encoder's LAST buffer; returns the frames written.
fn drain_loop(
    shared: &Shared,
    mapped: Vec<Mapped>,
    mut stream: File,
    mut ts: BufWriter<File>,
) -> u64 {
    let fd = shared.dev.as_raw_fd();
    let mut frames = 0;
    let mut idle_since = Instant::now();
    'drain: loop {
        let revents = poll(fd, libc::POLLIN | libc::POLLOUT, 100);
        let mut progress = false;
        loop {
            let mut planes = [zeroed::<Plane>(); 2];
            let mut b = buffer(OUTPUT, MEMORY_DMABUF, 0, &mut planes);
            if ioctl(fd, DQBUF, &mut b).is_err() {
                break;
            }
            shared.free.fetch_or(1 << b.index, Ordering::AcqRel);
            progress = true;
        }
        loop {
            let mut planes = [zeroed::<Plane>()];
            let mut b = buffer(CAPTURE, MEMORY_MMAP, 0, &mut planes);
            match ioctl(fd, DQBUF, &mut b) {
                Ok(()) => {}
                // EPIPE: the LAST buffer is already out.
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => break 'drain,
                Err(_) => break,
            }
            progress = true;
            let m = &mapped[b.index as usize];
            let len = (planes[0].bytesused as usize).min(m.1);
            // SAFETY: the driver filled `len` bytes of this mapping and won't
            // touch it again until it is re-queued below.
            let data = unsafe { std::slice::from_raw_parts(m.0.cast::<u8>(), len) };
            if let Err(e) = stream.write_all(data) {
                warn!("record: write failed, stopping: {e}");
                break 'drain;
            }
            if has_slice(data) {
                let key = micros(&b.timestamp);
                let mut queued = shared.queued.lock().unwrap();
                // Anything older than this frame never came out.
                while let Some(&(k, ns, dropped)) = queued.front().filter(|q| q.0 <= key) {
                    queued.pop_front();
                    if k == key {
                        let _ = writeln!(ts, "{ns} {dropped}");
                        break;
                    }
                }
                frames += 1;
            }
            if b.flags & BUF_FLAG_LAST != 0 {
                break 'drain;
            }
            if let Err(e) = ioctl(fd, QBUF, &mut b) {
                warn!("record: QBUF capture failed: {e}");
            }
        }
        if progress {
            idle_since = Instant::now();
        } else if shared.draining.load(Ordering::Acquire) {
            if !shared.capturing.load(Ordering::Acquire) {
                break;
            }
            if idle_since.elapsed() > Duration::from_secs(2) {
                warn!("record: encoder never signalled the end of the stream");
                break;
            }
        } else if revents & libc::POLLERR != 0 {
            // Nothing queued yet: poll returns at once.
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    for mut type_ in [OUTPUT, CAPTURE] {
        let _ = ioctl(fd, STREAMOFF, &mut type_);
    }
    let _ = ts.flush();
    let _ = stream.sync_all();
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_abi() {
        assert_eq!(QUERYCAP, 0x8068_5600);
        assert_eq!(ENUM_FMT, 0xc040_5602);
        assert_eq!(S_FMT, 0xc0d0_5605);
        assert_eq!(REQBUFS, 0xc014_5608);
        assert_eq!(QUERYBUF, 0xc058_5609);
        assert_eq!(QBUF, 0xc058_560f);
        assert_eq!(DQBUF, 0xc058_5611);
        assert_eq!(STREAMON, 0x4004_5612);
        assert_eq!(STREAMOFF, 0x4004_5613);
        assert_eq!(S_PARM, 0xc0cc_5616);
        assert_eq!(S_CTRL, 0xc008_561c);
        assert_eq!(ENCODER_CMD, 0xc028_564d);
        assert_eq!(S_SELECTION, 0xc040_565f);
        assert_eq!(size_of::<Plane>(), 64);
        assert_eq!(std::mem::offset_of!(Format, num_planes), 188);
        assert_eq!(std::mem::offset_of!(Buffer, planes), 64);
    }

    #[test]
    fn sizes_are_even_and_stride_friendly() {
        assert_eq!(record_size((1280, 2856), 1.0), (1280, 2856));
        assert_eq!(record_size((1280, 2856), 0.5), (640, 1428));
        assert_eq!(record_size((1224, 2700), 1.0), (1216, 2700));
        assert_eq!(record_size((1224, 2700), 0.25), (256, 674));
        assert_eq!(record_size((1280, 2856), 0.0), (128, 286));
        assert_eq!(record_size((1280, 2856), f64::NAN), (1280, 2856));
        assert_eq!(record_size((100, 100), 0.1), (64, 64));
    }

    #[test]
    fn parameter_sets_alone_are_not_a_frame() {
        let sps_pps = [0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 1, 0x68, 0xee];
        assert!(!has_slice(&sps_pps));
        assert!(has_slice(&[0, 0, 0, 1, 0x41, 0x9a]));
        assert!(has_slice(&[0, 0, 1, 0x65, 0x88]));
        let mut joined = sps_pps.to_vec();
        joined.extend([0, 0, 0, 1, 0x65, 0x88]);
        assert!(has_slice(&joined));
        assert!(!has_slice(&[]));
    }

    #[test]
    fn timestamps_keep_microseconds() {
        let t = timeval(12_345_678_901_234);
        assert_eq!((t.tv_sec, t.tv_usec), (12_345, 678_901));
    }
}
