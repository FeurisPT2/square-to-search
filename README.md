# Square to Search

Ứng dụng desktop thuần **Rust + Slint** mang trải nghiệm **Square to Search** (tìm ảnh tương tự, copy chữ, dịch nhanh, hỏi AI) và **Continuous Live Translate** (dịch truyện tranh liên tục khi cuộn trang) lên **Windows 10/11** và **Linux (X11 & Wayland)** — chạy gọn nhẹ ở System Tray, dùng ngay miễn phí không bắt buộc API key.

> **Pure Rust + Slint desktop utility** bringing *Square to Search* (reverse image search, local OCR, quick translate, multimodal Ask AI) and *Continuous Live Translate* (live manga/manhwa/comic translation overlay) to Windows 10/11 and Linux (X11 & Wayland). Free out of the box, with optional BYOK and local Ollama support.

---

## Tính năng chính / Core Features

### 1. Square to Search (Lướt web & Tra cứu nhanh)
- Bấm phím tắt (`Ctrl+Shift+S`), kéo khung vuông chọn vùng màn hình (chỉnh được 4 cạnh/góc; `Esc` hoặc chuột phải để hủy).
- **Tìm ảnh tương tự (Visual Search):** Upload trực tiếp ảnh vùng chọn tới **Google Lens** (mặc định), **Yandex Images**, hoặc **Bing Visual Search** và mở trên trình duyệt mặc định (không qua host lưu ảnh trung gian; tự động copy vào Clipboard để `Ctrl+V` nếu upload lỗi). Hoạt động ngay cả khi chưa tải model OCR.
- **Copy chữ (Local OCR):** Nhận diện văn bản cục bộ qua PP-OCRv5 ONNX với chuyển nhanh gói `CJK + EN`, `Tiếng Việt / Latin`, `Tiếng Hàn` ngay trên thẻ kết quả.
- **Dịch nhanh (Quick Translate):** Dịch tức thì vùng văn bản vừa chọn và đối chiếu bản gốc ngay tại chỗ.
- **Hỏi AI (Ask AI Streaming):** Gửi ảnh vùng chọn (PNG) + chữ OCR tới **Gemini**, **OpenAI-compatible**, hoặc **Ollama Local**, hỗ trợ chip gợi ý nhanh (**Giải thích**, **Tóm tắt**, **Dịch tự nhiên**), nhập câu hỏi tùy ý, hiển thị phản hồi streaming theo thời gian thực và nút **Dừng** để hủy ngay lập tức.

### 2. Continuous Live Translate (Đọc truyện tranh liên tục)
- Bấm `Ctrl+Shift+T` để chọn vùng đọc (hoặc cửa sổ trình đọc trên Linux Wayland), sau đó cuộn trang tự nhiên mà không cần bấm lại phím tắt.
- **Chống nhấp nháy & tự chụp lặp:** Lọc khác biệt khung hình + chờ đứng yên (debounce mặc định `350 ms`, tùy chỉnh được), tự động loại trừ cửa sổ phủ khỏi vùng chụp (`WDA_EXCLUDEFROMCAPTURE` trên Windows, chụp theo cửa sổ trên X11/Wayland).
- **Speech-Bubble Clustering:** Tự động gộp các dòng/cột chữ dọc thành bóng thoại hoàn chỉnh và sắp xếp theo hướng đọc (**Phải → Trái** cho manga Nhật hoặc **Trái → Phải** cho manhwa/comic).
- **Hai chế độ hiển thị:**
  - **Phủ đè trực tiếp (In-place Overlay):** Cửa sổ trong suốt xuyên chuột, che chữ gốc và đặt bản dịch vào đúng bóng thoại.
  - **Bảng dịch nổi (Floating Panel):** Cửa sổ nổi kéo thả được, liệt kê bản dịch theo thứ tự đọc mà không che nét vẽ gốc.
- **Thanh điều khiển mini (Live Controls):** Tạm dừng/Tiếp tục, Ẩn/Hiện nhanh (`Ctrl+Shift+H`), chuyển chế độ In-place ↔ Bảng nổi, chọn lại vùng, hoặc dừng phiên Live.

---

## Phím tắt mặc định / Default Hotkeys

Có thể tùy chỉnh trong **Settings → Chung** (`config.toml`):

| Phím tắt mặc định | Chức năng / Action |
|---|---|
| `Ctrl+Shift+S` | Mở chế độ **Square to Search** |
| `Ctrl+Shift+T` | Bật / tắt **Continuous Live Translate** |
| `Ctrl+Shift+H` | Ẩn / hiện nhanh lớp phủ **Live Translate** |

---

## Ma trận hỗ trợ nền tảng / Platform Support Matrix (Decision 2A & 3A)

> **v1 hỗ trợ Windows 10/11 (`x86_64-pc-windows-msvc`) và Linux (`x86_64-unknown-linux-gnu`). Chưa hỗ trợ macOS ở v1.**

| Nền tảng / Display Server | Square to Search | Live: Floating Panel | Live: In-place Overlay (Click-through) | Ghi chú kỹ thuật |
|---|---|---|---|---|
| **Windows 10 (2004+) / 11** | Đầy đủ | Đầy đủ | **Đầy đủ** (cửa sổ thả nổi & toàn màn hình) | `WS_EX_LAYERED \| WS_EX_TRANSPARENT` + `WDA_EXCLUDEFROMCAPTURE` |
| **Linux X11** | Đầy đủ | Đầy đủ | **Đầy đủ** (cửa sổ thả nổi & toàn màn hình) | XShape `ShapeInput` rỗng + chụp theo cửa sổ trình đọc |
| **Linux Wayland — KDE Plasma / Hyprland / Sway (`wlr-layer-shell`)** | Đầy đủ (`ashpd` portal) | Đầy đủ | **Chỉ khi trình đọc toàn màn hình / maximized** | Phím tắt qua `GlobalShortcuts`; chụp cửa sổ qua PipeWire `ScreenCast`. Cửa sổ thả nổi tự chuyển sang Floating Panel kèm thông báo |
| **Linux Wayland — GNOME (Mutter)** | Đầy đủ (`Screenshot` / focus monitor) | Đầy đủ | Không hỗ trợ (tự động dùng **Floating Panel**) | GNOME Mutter không hỗ trợ `wlr-layer-shell`; Settings hiển thị thông báo rõ |

### Tăng tốc OCR (Decision 3A — Zero CUDA in v1)
- Thiết bị suy luận PP-OCRv5 trong **Settings → OCR & Models**: **`Auto`**, **`CPU`**, **`DirectML (Windows)`**.
- **Không đóng gói CUDA trong v1:** Không có dependency, feature flag, tùy chọn UI, `.dll` hay `.so` liên quan tới CUDA/cuDNN/cuBLAS/NCCL (`deny.toml` và script đóng gói đều kiểm tra chặn tuyệt đối).

---

## Mô hình OCR PP-OCRv5 & Tải lần đầu (SHA-256 Verified)

- Bộ cài giữ kích thước gọn nhẹ, **không nhúng sẵn trọng số ONNX nặng**.
- Ở lần chạy OCR / Live Translate đầu tiên khi chưa có model, hộp thoại **1-click** tải gói khởi đầu **`PP-OCRv5 mobile (CJK + EN)`** (~21–25 MB) với kiểm tra toàn vẹn **SHA-256** theo manifest ghim sẵn (`pp-ocrv5-oar-0.11.0`).
- Các gói ngôn ngữ hỗ trợ trong **Settings → OCR & Models**:
  - `CJK + EN (Unified)` (`mobile` mặc định / `server` độ chính xác cao hơn)
  - `Korean` (`mobile` / `server`)
  - `Vietnamese / Latin` (`mobile` / `server`)
- Hỗ trợ **nhập thư mục model offline** từ ổ đĩa và **tự động giải phóng RAM khi nhàn rỗi** (mặc định `5 phút`).

---

## Dịch thuật & AI: Miễn phí, BYOK (OS Keyring) & Ollama Local

Cấu hình tại **Settings → Dịch & AI**:

1. **Mặc định miễn phí (Không cần API key):**
   - Dùng endpoint web công khai của **Google Translate** kèm dự phòng **Bing Translator**.
   - Toàn bộ bóng thoại trên mỗi trang truyện được gom thành **1 batch request** kết hợp **LRU cache** để giảm thiểu giới hạn tần suất (`HTTP 429`).
2. **BYOK (Bring Your Own Key) — Bảo mật qua OS Keyring:**
   - **Google Gemini:** mặc định `gemini-2.5-flash` (gửi key qua header `x-goog-api-key`, hỗ trợ dịch batch + Ask AI streaming đa phương thức).
   - **OpenAI-compatible:** hỗ trợ Base URL tùy chỉnh (mặc định `https://api.openai.com/v1`, model `gpt-4o-mini`), dịch batch + Ask AI streaming SSE.
   - **DeepL:** hỗ trợ cả **DeepL API Free** (`api-free.deepl.com`, mặc định) và **DeepL Pro** (`api.deepl.com`) cho tác vụ dịch.
   - **Bảo mật API Key:** Toàn bộ API key được lưu trực tiếp trong **Windows Credential Manager** hoặc **Linux Secret Service** (`keyring`, service `square-to-search`). Key **không bao giờ** được ghi plain-text vào `config.toml`, không xuất hiện trong log/debug, và không hiển thị lại trên giao diện Slint.
3. **Ollama Local (Không cần API key):**
   - Kết nối tới `http://localhost:11434` (model mặc định `qwen2.5vl:7b`) cho cả dịch thuật và Hỏi AI đa phương thức cục bộ.
4. **Prompt dịch truyện tùy chỉnh & Kiểm tra kết nối:**
   - Cho phép nhập prompt hướng dẫn văn phong dịch manga/comic cho LLM và bấm **Kiểm tra Dịch Live** / **Kiểm tra Hỏi AI** ngay trong Settings.

---

## Kiến trúc Workspace / Cargo Workspace

```text
square-to-search/
├── Cargo.toml                 # Workspace root (Rust 1.100.0, edition 2024)
├── deny.toml                  # cargo-deny: advisories, licenses, CUDA crate bans, sources
├── scripts/
│   ├── use-tools.ps1          # Nạp .tools/bin & Cargo bin vào PATH (Windows PowerShell)
│   ├── use-tools.sh           # Nạp .tools/bin & Cargo bin vào PATH (Linux/bash)
│   ├── package-windows.ps1    # Đóng gói ZIP portable Windows x64 + DirectML.dll + SHA-256
│   └── package-linux.sh       # Đóng gói tarball + AppDir/.desktop Linux x64 + SHA-256
└── crates/
    ├── sts-core/              # Domain types, config.toml, SecretSlot, HiDPI geometry, bubble clustering, LRU cache
    ├── sts-platform/          # OS capture, hotkeys, tray, overlay (Decision 2A), keyring SecretStore, ashpd Wayland portals
    ├── sts-ocr/               # PP-OCRv5 ONNX (oar-ocr/ort CPU + DirectML), SHA-256 manifest, idle unload
    ├── sts-translate/         # Free Web (Google/Bing), Gemini, OpenAI-compat, DeepL, Ollama, streaming VisionAI, factory
    ├── sts-search/            # Reverse image search (Google Lens, Yandex, Bing + clipboard fallback)
    └── sts-app/               # Binary entry, Tokio runtime, AppController, Slint UI (.slint)
```

---

## Phát triển & Kiểm thử / Development & Verification

Yêu cầu Rust toolchain `1.100.0` (đã ghim trong `rust-toolchain.toml`) và Slint `1.19.0+`.

### Windows (PowerShell)

```powershell
# 1. Nạp Cargo & project tools (.tools\bin) vào PATH
. .\scripts\use-tools.ps1

# 2. Chạy ứng dụng ở chế độ dev
cargo run -p sts-app

# 3. Kiểm tra toàn bộ workspace (fmt, clippy, nextest, Slint, cargo-deny)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
.\.tools\bin\cargo-nextest.exe nextest run --workspace
.\.tools\bin\slint-viewer.exe --check "crates\sts-app\ui\app.slint"
.\.tools\bin\cargo-deny.exe check
```

### Linux (Bash)

```bash
# 1. Nạp Cargo & project tools (.tools/bin) vào PATH
. ./scripts/use-tools.sh

# 2. Build & kiểm tra
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

---

## Đóng gói phát hành / Release Packaging

### Windows x64 Portable (`dist\square-to-search-windows-x64.zip`)

```powershell
. .\scripts\use-tools.ps1
powershell -ExecutionPolicy Bypass -File "scripts\package-windows.ps1"
# Hoặc nếu đã build `cargo build --release -p sts-app` trước đó:
powershell -ExecutionPolicy Bypass -File "scripts\package-windows.ps1" -SkipBuild
```

Script thực hiện:
- Build `sts-app.exe` bản release (trừ khi truyền `-SkipBuild`).
- Stage vào `dist\staging\square-to-search-windows-x64\`, **tự động dereference symlink `target\release\DirectML.dll`** thành file DLL thực tế (`Length > 0`, fail nếu thiếu hoặc `0` byte) cùng mọi runtime DLL hợp lệ khác và `README.md`.
- Chặn tuyệt đối mọi DLL liên quan tới CUDA (`cuda|cudnn|cublas|cufft|curand|cusolver|cusparse|nccl|nvrtc`) ở cả `target\release\` và thư mục staging (Decision 3A).
- Tạo `dist\square-to-search-windows-x64.zip` và `dist\square-to-search-windows-x64.zip.sha256`.

### Linux x64 Portable Tarball & AppDir (`dist/square-to-search-linux-x64.tar.gz`)

```bash
. ./scripts/use-tools.sh
bash scripts/package-linux.sh
# Hoặc bỏ qua bước build:
bash scripts/package-linux.sh --skip-build
```

Script thực hiện:
- Build `target/release/sts-app` (trừ khi truyền `--skip-build`).
- Stage đồng thời tarball portable (`dist/square-to-search-linux-x64.tar.gz` + `.sha256`) và cấu trúc AppDir (`dist/AppDir/usr/bin/sts-app`, `usr/lib/`, `square-to-search.desktop`, `square-to-search.svg`, `AppRun`).
- Dereference các shared library (như `libonnxruntime*.so*`) qua `cp -L` và abort nếu phát hiện bất kỳ `.so` CUDA/cuDNN/cuBLAS/NCCL nào.

---

## Giấy phép / License

Dual-licensed under **MIT OR Apache-2.0** (see `Cargo.toml`).
