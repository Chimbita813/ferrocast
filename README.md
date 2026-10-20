# Chimcast

A lightweight streaming and recording app for Windows, written in Rust. It is meant
to replace OBS/Streamlabs Desktop for the common case, using as little memory
and CPU as possible.

There is no GStreamer, FFmpeg, Electron or browser runtime. Chimcast talks to Windows directly:

| Stage | How |
|---|---|
| Display and window capture | Windows.Graphics.Capture. Frames stay on the GPU as D3D11 textures |
| Webcam | Media Foundation source reader |
| Images | WIC, decoded once into a GPU texture |
| Compositing | Chimcast's own D3D11 quad renderer, double-buffered canvas |
| Colour conversion | `ID3D11VideoProcessor` (the GPU's fixed-function video engine), BGRA to NV12 BT.709 |
| Video encoding | Media Foundation H.264: the hardware encoder (NVENC / AMF / Quick Sync) fed NV12 textures with no copy. Falls back to the Microsoft software encoder |
| Audio | WASAPI shared mode: microphones, plus desktop audio via loopback. Mixed in Rust at 48 kHz stereo |
| Audio encoding | Media Foundation AAC-LC |
| Recording | Chimcast's own progressive MP4 muxer |
| Streaming | Chimcast's own RTMP client (Twitch, YouTube, Kick, any `rtmp://` server) |
| UI | egui, drawn by a small D3D11 renderer on the same device, so the preview is the canvas texture itself |

Encoders only exist while you are recording or streaming. When idle, the app is
just capture, compositing and a 30 fps preview.

## Requirements

- Windows 10 version 1903 or later (Windows 11 hides the yellow capture border)
- A GPU with Direct3D 11 (any GPU from the last ten years)

## Build

```powershell
# Install Rust from https://rustup.rs (MSVC toolchain), then:
cargo build --release
.\target\release\chimcast.exe
```

CI builds `chimcast.exe` on every push. Download it from the run's artifacts.

## Using it

- **Scenes**: `+` adds a scene, click to switch, and the text box renames the current one.
- **Sources**: `+` adds a display, a window, a webcam, an image or a colour. Click a source in the
  preview to select it. Drag to move it, drag the red corner handle to resize it (hold Shift to
  ignore aspect ratio), and use **Fit** to fill the canvas at the source's aspect ratio.
- **Audio mixer**: per-source meter, volume in dB and mute. Desktop Audio and the default mic are
  there by default.
- **Settings**: resolution, frame rate, bitrates, recording folder, stream service and key.
- The status bar shows fps, render time, the active encoder and the app's private memory.

Settings and scenes are saved to `%APPDATA%\Chimcast\project.json`, and the log goes to
`%APPDATA%\Chimcast\chimcast.log`. The stream key is stored there in plain text.

## Layout

```
crates/
  chimcast-core   project model (scenes, sources, settings), serde, no OS code
  chimcast-media  H.264/AAC helpers, MP4 + FLV muxers, RTMP client, audio mixer, output threads
  chimcast-win    Windows engine: capture, compositor, NV12, MF encoders, WASAPI
  chimcast        the app: winit window, egui UI, D3D11 painter
```

`chimcast-core` and `chimcast-media` are platform-independent. Their tests run anywhere, and the
interop tests check the MP4/FLV output and an RTMP publish against FFmpeg:

```
cargo test -p chimcast-core -p chimcast-media
```

## Known gaps (next up)

- `rtmps://` (TLS) streaming. Twitch and YouTube accept plain `rtmp://`, Facebook needs TLS.
- Per-source crop, filters (chroma key, noise suppression), transitions between scenes.
- Following the Windows default audio device when it changes while running.
- Recording to MKV/fragmented MP4 so a crash doesn't lose the file.
- A file picker for image sources (currently a path text box).

## Updates

Every green build of `main` is published as a GitHub release (`build-<n>`) with
`chimcast.exe` and its SHA-256. When Chimcast starts it checks the latest
release; if it's newer, it downloads it, verifies the checksum, swaps the exe
and restarts into it. If you're already recording or streaming, the new build
waits for the next launch. Turn it off under Settings → App → Updates.
Local builds (no `CHIMCAST_BUILD`) never update themselves. The repository must
be public for the app to see releases without a token.
