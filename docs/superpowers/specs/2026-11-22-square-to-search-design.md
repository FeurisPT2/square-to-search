# Thiết kế hệ thống: `square-to-search`

- **Ngày:** 2026-11-22
- **Nền tảng mục tiêu:** PC — Windows 10/11 (`x86_64-pc-windows-msvc`) và Linux (`x86_64-unknown-linux-gnu`, X11 & Wayland)
- **Công nghệ cốt lõi:** Rust (`1.100.0`, pin tại `rust-toolchain.toml`) + Slint (`1.19+`, pure Rust declarative UI)
- **Phạm vi công việc:** Dự án đầy đủ (Full Project)

---

## 1. Mục tiêu sản phẩm & Lý do xây dựng (Intent & Why)

`square-to-search` mang trải nghiệm tiện lợi kiểu "Circle to Search" lên máy tính Windows và Linux, phục vụ hai tình huống sử dụng thực tế hàng ngày của người dùng cuối:

1. **Live Translate khi đọc truyện tranh (manga / manhwa / manhua / comic):** Người dùng bật chế độ dịch liên tục một lần cho vùng đọc hoặc cửa sổ trình đọc; mỗi khi cuộn hoặc lật sang trang mới, ứng dụng tự động nhận diện chữ và cập nhật bản dịch tiếng Việt mà không phải bấm lại phím tắt hay rời khỏi trang truyện.
2. **Square to Search khi lướt web:** Khi bắt gặp hình ảnh, sản phẩm, hoặc đoạn chữ cần tra cứu, người dùng bấm phím tắt, kéo một khung vuông chọn vùng trên màn hình và thao tác ngay lập tức: tìm ảnh tương tự, copy chữ, dịch nhanh hoặc hỏi AI.

Ứng dụng được định hướng như một sản phẩm desktop hoàn chỉnh có giao diện **Settings** cho người dùng cuối: **dùng ngay miễn phí không bắt buộc API key** (bộ cài gọn nhẹ, hướng dẫn tải gói OCR starter ~25MB bằng 1 click ở lần OCR đầu tiên), đồng thời cho phép người dùng nâng cao cắm thêm API key riêng (BYOK) hoặc Ollama local khi muốn chất lượng dịch truyện tự nhiên hơn và dùng tính năng Hỏi AI.

---

## 2. Goals, Non-Goals & Anti-Goals

### Goals
- Ứng dụng native thuần Rust + Slint chạy trên Windows 10/11 và Linux (X11 + Wayland), khởi động nhanh, gọn nhẹ, chạy nền dưới System Tray.
- **Live Translate liên tục** cho truyện tranh dạng ảnh (Nhật / Hàn / Trung / Anh → tiếng Việt) với **cả 2 chế độ hiển thị** chọn trong Settings:
  - **Phủ đè trực tiếp lên khung thoại (In-place Overlay):** cửa sổ trong suốt xuyên chuột, vẽ nền đồng màu che chữ gốc và đặt bản dịch đúng vị trí từng bóng thoại.
  - **Bảng dịch nổi tách riêng (Floating Panel):** bảng nổi trong suốt di chuyển được, liệt kê các câu dịch theo đúng thứ tự đọc mà không che nét vẽ gốc.
- **Square to Search** khi lướt web: kéo khung vuông → hiện thanh công cụ nổi ngay bên dưới vùng chọn:
  - **Copy chữ / Dịch nhanh / Hỏi AI:** hiển thị kết quả ngay tại chỗ trong thẻ nổi Slint (hỗ trợ cả OCR tiếng Việt khi lướt web).
  - **Tìm ảnh tương tự (Google Lens mặc định, hỗ trợ Yandex / Bing):** mở kết quả trên trình duyệt mặc định (hoạt động ngay cả khi chưa tải model OCR).
- **Mô hình Hybrid cho OCR / Dịch / AI:**
  - OCR chạy **local** (PP-OCRv5 ONNX qua ONNX Runtime, tăng tốc CPU + DirectML trên Windows), tải gói model theo ngôn ngữ từ manifest có kiểm tra SHA-256, trả tọa độ chính xác cho từng cụm thoại và chạy thoải mái không tốn phí hay dính rate-limit khi đọc truyện dài.
  - Dịch mặc định **miễn phí không cần API key** thông qua endpoint web miễn phí (Google Translate, dự phòng Bing) kèm gom batch theo trang + LRU cache, cộng thêm tuỳ chọn trong Settings cho BYOK (Gemini, OpenAI-compatible, DeepL) và **Ollama local**.
  - Tính năng **Hỏi AI** dùng provider AI do người dùng cấu hình (Gemini / OpenAI-compatible / Ollama).

### Non-Goals (ngoài phạm vi v1)
- **macOS:** Thiết kế giữ ranh giới tầng nền tảng sạch để mở rộng về sau, nhưng không build hay kiểm thử macOS ở v1.
- **Tăng tốc GPU CUDA trong bản v1 *(Đã chốt: 3A)*:** Bản v1 chỉ đóng gói **CPU + DirectML (Windows)** để bộ cài luôn gọn nhẹ và không cần CUDA Toolkit; CUDA sẽ tách thành gói plugin tải thêm hoặc bản phát hành `-cuda` riêng sau v1.
- **Compositor-specific geometry IPC trên Linux Wayland *(Đã chốt: 2A)*:** Không viết mã lấy tọa độ cửa sổ thả nổi riêng cho từng compositor (KWin/Hyprland/Sway) trong v1.
- **Nhúng trình duyệt/WebView trong app:** Không nhúng WebView để render Google Lens bên trong Slint; tìm ảnh luôn mở trên trình duyệt mặc định của người dùng.
- **AI Inpainting / vẽ lại nền truyện:** Chế độ phủ đè dùng hộp nền bo góc lấy màu thích ứng từ vùng thoại, không chạy mô hình inpainting xóa chữ phức tạp.
- **Hook game fullscreen độc quyền / nội dung DRM chống chụp màn hình.**
- **Tài khoản người dùng, đồng bộ đám mây hay telemetry.**

### Anti-Goals (những thứ biến sản phẩm thành thất bại dù chạy được)
- **Vòng lặp tự chụp bản dịch (Self-capture loop) hoặc nhấp nháy overlay:** Nếu bộ chụp màn hình chụp lại chính chữ tiếng Việt vừa phủ lên, OCR sẽ dịch lặp vô hạn hoặc làm overlay chớp tắt gây khó chịu khi đọc.
- **Nặng máy / cồng kềnh phụ thuộc ngoài:** Không bắt người dùng cài Python, Node.js hay CUDA toolkit; bộ cài gọn nhẹ; không chiếm RAM cao (> 500MB) khi đang nằm chờ ở System Tray.
- **Gọi OCR/Dịch loạn xạ lúc đang cuộn trang:** Không xử lý mỗi khung hình khi người dùng đang kéo chuột; bắt buộc lọc khác biệt khung hình + chờ màn hình đứng yên (debounce).
- **Âm thầm upload ảnh/dữ liệu màn hình lên bên thứ ba không minh bạch:** Không đẩy ảnh lên host lưu ảnh công cộng bên thứ ba ngoài luồng Tìm ảnh đã chọn, và hiển thị rõ ràng trong Settings khi dùng endpoint dịch web mặc định.

---

## 3. Ràng buộc kỹ thuật & Nền tảng (Constraints)

### 3.1. Chống tự chụp bản dịch, Cửa sổ xuyên chuột & Ma trận nền tảng *(Đã chốt: 2A)*
- **Slint (`1.19+`):** Cửa sổ overlay dùng `no-frame: true; background: #00000000;` kết hợp `WinitWindowAccessor` để truy cập handle cửa sổ native.
- **Windows 10 (2004+) / Windows 11 — Hỗ trợ đầy đủ cả In-place (cửa sổ thả nổi/vùng chọn) & Floating Panel:**
  - Xuyên chuột bằng `WS_EX_LAYERED | WS_EX_TRANSPARENT`.
  - Loại trừ cửa sổ overlay khỏi mọi lần chụp màn hình bằng `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` → chụp liên tục không bao giờ dính overlay và không nhấp nháy.
- **Linux X11 — Hỗ trợ đầy đủ cả In-place (cửa sổ thả nổi/vùng chọn) & Floating Panel:**
  - Xuyên chuột bằng vùng input rỗng qua XShape (`XShapeCombineRectangles` với `ShapeInput`).
  - Tránh self-capture bằng cách chụp trực tiếp cửa sổ trình đọc truyện (XComposite / capture theo Window ID), không chụp chồng lên cửa sổ overlay.
- **Linux Wayland — Phân tách rõ theo giới hạn giao thức *(Đã chốt: 2A)*:**
  - Phím tắt toàn cục dùng `xdg-desktop-portal` (`GlobalShortcuts` qua `ashpd`, hỗ trợ GNOME 48+, KDE Plasma 5.27+, wlroots portals).
  - Chụp liên tục dùng `xdg-desktop-portal` (`ScreenCast` + PipeWire) chụp theo cửa sổ trình đọc truyện → tự nhiên tránh self-capture.
  - **Live Translate — Floating Panel:** Chạy đầy đủ trên mọi Wayland compositor (GNOME, KDE, Hyprland, Sway…).
  - **Live Translate — In-place Overlay:** Do PipeWire không trả về tọa độ `(x, y)` của cửa sổ thả nổi, trên Wayland v1 **chỉ bật chế độ In-place khi cửa sổ trình đọc đang ở chế độ toàn màn hình / maximized** trên các compositor hỗ trợ `wlr-layer-shell` (KDE Plasma, Hyprland, Sway/wlroots). Trên GNOME Mutter (không hỗ trợ `wlr-layer-shell`) hoặc khi đọc dạng cửa sổ thả nổi trên Wayland, Settings giải thích rõ và tự động chuyển sang **Floating Panel**.
  - **Square to Search trên GNOME Wayland đa màn hình:** Chụp và mở lớp chọn vùng trên màn hình đang focus (hoặc ủy quyền bước chụp cho `xdg-desktop-portal` Screenshot portal) thay vì cố trải một cửa sổ `xdg_toplevel` xuyên nhiều màn hình.

### 3.2. Multi-monitor & HiDPI Scaling
- Mọi tọa độ phải tách biệt rõ hai hệ quy chiếu: **tọa độ vật lý (physical pixels)** từ bộ chụp màn hình & OCR, và **tọa độ logic (logical pixels)** theo `scale_factor` của từng màn hình khi hiển thị trên Slint.

### 3.3. Đặc thù chữ truyện tranh (CJK & chữ dọc) & Quản lý gói OCR ONNX *(Đề xuất kỹ thuật — Architect Call)*
- **Gom bóng thoại:** OCR cấp dòng (DBNet + SVTR của PP-OCRv5) trả về từng dòng/cột riêng lẻ. Bắt buộc có bước **Speech-Bubble Clustering** để gộp các dòng thuộc cùng một khung thoại thành khối văn bản hoàn chỉnh trước khi dịch, đồng thời sắp xếp thứ tự đọc phù hợp (phải→trái cho manga Nhật, trái→phải cho manhwa/comic).
- **Nguồn model & phân gói *(Architect Call)*:**
  - Dùng các file ONNX PP-OCRv5 đã được chuyển đổi sẵn từ nguồn upstream đáng tin cậy (tương thích `oar-ocr` / Hugging Face hoặc GitHub Releases của dự án), quản lý qua **manifest gắn sẵn trong app** ghi rõ version, URL và mã kiểm tra **SHA-256** (hỗ trợ nhập thư mục model offline nếu người dùng muốn).
  - **Phân gói ngôn ngữ:**
    - `CJK + EN (Unified PP-OCRv5)`: Tiếng Trung, Tiếng Nhật, Tiếng Anh (gói mặc định cho đọc truyện + lướt web).
    - `Korean (korean_PP-OCRv5)`: Gói nhận diện riêng cho manhwa tiếng Hàn.
    - `Latin / Vietnamese (latin_PP-OCRv5)`: Gói nhận diện riêng hỗ trợ chính xác dấu tiếng Việt khi dùng Square to Search lúc lướt web.
  - **Hai mức độ chính xác/tốc độ:** `mobile` *(mặc định, ~16–25MB/gói — nhanh trên CPU, nhẹ RAM)* và `server` *(tuỳ chọn tải thêm trong Settings, ~85MB+ — đọc font truyện cách điệu tốt hơn)*.
  - **Trải nghiệm lần chạy đầu *(Architect Call)*:** Bộ cài không nhúng sẵn model nặng để giữ kích thước gọn; khi người dùng chạy OCR/Live Translate lần đầu mà chưa có model, app hiện hộp thoại 1-click tự động tải gói `mobile` `CJK + EN` (~25MB) kèm thanh tiến trình. Tính năng **Tìm ảnh (Google Lens)** vẫn chạy ngay lập tức không cần chờ tải model.

### 3.4. Bộ dịch mặc định miễn phí *(Đã chốt: 1A)*
- Sử dụng endpoint web miễn phí của **Google Translate** (`translate_a/single`, có fallback sang endpoint web của **Bing Translator**).
- Để giảm thiểu tối đa rủi ro rate-limit khi đọc truyện liên tục:
  - Luôn **gom toàn bộ các bóng thoại trên cùng một trang thành 1 batch request** (ngăn cách bằng delimiter an toàn) thay vì gửi từng bóng thoại lẻ.
  - Kết hợp **LRU cache** theo `(hash_text, source_lang, target_lang)` để không gọi mạng lại khi cuộn lên/xuống nhẹ.
  - Hiển thị minh bạch trong **Settings → Dịch & AI** rằng chế độ mặc định dùng endpoint dịch web công khai (có thể bị giới hạn nếu đọc dồn dập), kèm thông báo gọn + nút chuyển nhanh sang BYOK (Gemini / OpenAI / DeepL) hoặc Ollama local nếu gặp lỗi `429`/rate-limit.

### 3.5. Tìm ảnh ngược (Reverse Image Search) ra trình duyệt mặc định
- Luồng chính: POST trực tiếp ảnh vùng chọn lên endpoint upload của Google Lens (`https://lens.google.com/v3/upload`) — hoặc Yandex / Bing theo cấu hình — lấy URL kết quả và mở bằng trình duyệt mặc định (`open` crate). Không upload qua host ảnh bên thứ ba.
- Phương án dự phòng (Fallback): Nếu endpoint bị chặn/thay đổi hoặc lỗi mạng, tự động copy ảnh vùng chọn vào clipboard, mở `https://lens.google.com` và hiện thông báo gọn nhắc người dùng nhấn `Ctrl+V`.

---

## 4. Các hướng kiến trúc đã cân nhắc (Approaches)

### Hướng 1 *(Đã chọn)*: Một ứng dụng Rust + Slint duy nhất, nhúng ONNX Runtime (`ort` / `oar-ocr`)
- Cargo workspace phân tầng rõ ràng (`core`, `platform`, `ocr`, `translate`, `search`, `app`), biên dịch thành một ứng dụng desktop duy nhất không cần Python/Node.
- Chạy model PP-OCRv5 local qua ONNX Runtime (`oar-ocr` / `ort`) với **CPU mặc định và DirectML trên Windows** *(CUDA tách riêng sau v1 theo 3A)*, tự động giải phóng session OCR khi nhàn rỗi (idle timeout) để giảm RAM.
- **Đánh đổi:** Tốc độ OCR cao cho trải nghiệm cày truyện liên tục, hệ sinh thái PP-OCRv5 cho CJK chín muồi; cần đóng gói kèm thư viện động ONNX Runtime (`.dll` / `.so`) trong bản phát hành (gọn nhẹ vì không kèm CUDA/cuDNN).

### Hướng 2 *(Không chọn)*: Thuần Rust 100% không C++ (`ocrs-cjk` / `rten` / `tract`)
- Kiến trúc tương tự Hướng 1 nhưng thay ONNX Runtime bằng engine suy luận thuần Rust.
- **Đánh đổi:** Đóng gói 1 file binary duy nhất cực gọn, không lo `.dll`/`.so`; nhưng chỉ chạy CPU, chậm hơn `ort` khoảng 2–4 lần trên trang manga nhiều chữ (gây cảm giác khựng khi đọc Live Translate) và hệ sinh thái model CJK mới hơn.

### Hướng 3 *(Không chọn)*: App Slint nền siêu nhẹ + Tiến trình OCR Worker riêng qua IPC
- App chính chỉ giữ tray + hotkey + overlay (~20–30MB RAM), sinh tiến trình con riêng để nạp model OCR khi cần rồi tắt hẳn khi xong.
- **Đánh đổi:** Cách ly sự cố tốt và trả lại 100% RAM ngay lập tức; nhưng tăng độ phức tạp về giao thức IPC, quản lý vòng đời tiến trình và debug đa nền tảng, trong khi cơ chế tự giải phóng session OCR theo idle timeout ở Hướng 1 đã giải quyết phần lớn bài toán RAM gọn hơn nhiều.

**Lý do chọn Hướng 1:** Người dùng đã chốt Hướng 1 vì cân bằng tốt nhất giữa độ mượt khi đọc truyện tranh liên tục, trải nghiệm cài-là-dùng không phụ thuộc ngoài, và độ gọn dễ bảo trì của codebase Rust.

---

## 5. Thiết kế chi tiết

### 5.1. Kiến trúc Cargo Workspace *(Architect Call)*

```text
square-to-search/
├── Cargo.toml                  # Cargo workspace root
├── crates/
│   ├── sts-core/               # Domain types, config TOML, frame-diff + debounce, bubble clustering, LRU cache
│   ├── sts-platform/           # Platform traits + Windows (Win32/DXGI/WGC) & Linux (X11/Wayland Portal/PipeWire)
│   ├── sts-ocr/                # ONNX Runtime (oar-ocr / ort), SHA-256 manifest, downloader, mobile/server tiers, idle unload
│   ├── sts-translate/          # Translator & VisionAiProvider traits (Google/Bing web free default, Gemini, OpenAI, DeepL, Ollama)
│   ├── sts-search/             # Visual search launcher (Google Lens direct upload, Yandex, Bing + clipboard fallback)
│   └── sts-app/                # Binary entry, Tokio runtime, System Tray, Slint UI (.slint files)
└── crates/sts-app/ui/
    ├── snip-overlay.slint      # Fullscreen vùng chọn + thanh công cụ nổi + thẻ kết quả nhanh
    ├── live-inplace.slint      # Lớp phủ xuyên chuột vẽ bản dịch trực tiếp lên khung thoại
    ├── live-panel.slint        # Bảng dịch nổi tách riêng
    ├── live-controls.slint     # Thanh điều khiển mini khi bật Live (Pause, Ẩn/Hiện, Đổi chế độ, Dừng)
    ├── first-run-ocr.slint     # Hộp thoại 1-click tải gói OCR starter khi dùng OCR lần đầu
    └── settings.slint          # Giao diện Cài đặt (5 tab)
```

### 5.2. Luồng hoạt động người dùng

#### A. Chạy nền & Phím tắt toàn cục
- App chạy nền ở System Tray (kèm menu: *Square to Search*, *Live Translate*, *Settings*, *Quit*).
- *Đề xuất phím tắt mặc định (tuỳ chỉnh trong Settings — Architect Call):*
  - `Ctrl+Shift+S`: Mở chế độ **Square to Search**.
  - `Ctrl+Shift+T`: Bật / tắt **Live Translate**.
  - `Ctrl+Shift+H`: Ẩn / hiện nhanh lớp dịch Live (liếc xem tranh gốc mà không tắt phiên Live).

#### B. Chế độ 1 — Continuous Live Translate (Đọc truyện)
1. Người dùng bấm phím tắt Live Translate → chọn vùng đọc trên màn hình (Windows / Linux X11) hoặc chọn cửa sổ trình đọc truyện qua portal (Linux Wayland; khuyến khích chọn cửa sổ trên X11 để tránh self-capture).
2. Vòng lặp chụp nền lấy mẫu nhẹ (~4–6 Hz) và tính độ khác biệt khung hình (downscaled pixel-grid diff) trên vùng mục tiêu:
   - Khi hình đang cuộn liên tục: không kích hoạt OCR/Dịch.
   - Khi hình thay đổi so với lần dịch trước **và đứng yên liên tục trong khoảng debounce** *(Architect Call: mặc định 350ms, chỉnh được trong Settings)*: chạy pipeline xử lý bất đồng bộ (hủy tác vụ cũ nếu người dùng lại cuộn tiếp):
     ```text
     CaptureFrame
       → Local OCR (PP-OCRv5 → Vec<OcrLine>)
       → Speech-Bubble Clustering (gom dòng ngang/dọc → Vec<TextBlock>, xếp thứ tự đọc RTL/LTR)
       → Translation Cache Lookup (LRU theo hash text + cặp ngôn ngữ)
       → Batch Translate (gom các bóng thoại trong trang thành 1 request)
       → Render (In-place Overlay hoặc Floating Panel)
     ```
3. **Hiển thị theo thiết lập người dùng chọn trong Settings** *(đã chốt: hỗ trợ cả hai; Wayland áp dụng quy tắc 2A)*:
   - **Phủ đè trực tiếp (In-place):** Vẽ hộp nền bo góc che từng `TextBlock`, lấy màu nền thích ứng từ viền vùng thoại (chữ sáng/tối tương phản tự động), tự động điều chỉnh cỡ chữ để bản dịch tiếng Việt nằm gọn trong khung thoại. Lớp phủ xuyên chuột để người dùng cuộn truyện tự nhiên.
   - **Bảng dịch nổi (Floating Panel):** Cửa sổ nổi trong suốt kéo thả được, hiển thị danh sách các bóng thoại đã dịch theo số thứ tự đọc, có nút xem chữ gốc và copy nhanh.
4. **Thanh điều khiển mini (Live Controls):** Một thanh nổi nhỏ luôn bấm được để *Tạm dừng/Tiếp tục*, *Ẩn/Hiện bản dịch*, *Chuyển nhanh In-place ↔ Bảng nổi* (nếu nền tảng hỗ trợ), *Chọn lại vùng*, hoặc *Dừng Live*.

#### C. Chế độ 2 — Square to Search (Lướt web tìm nhanh)
1. Bấm phím tắt → chụp đóng băng màn hình hiện tại (freeze-frame; trên GNOME Wayland đa màn hình lấy màn hình đang focus hoặc qua Screenshot portal) và hiện lớp phủ Slint phủ mờ nhẹ; người dùng kéo chọn khung hình chữ nhật (chỉnh được 4 góc/cạnh, `Esc` hoặc click phải để hủy).
2. Ngay khi thả chuột, **thanh công cụ nổi** xuất hiện bám sát dưới vùng chọn (tự kẹp biên trong viewport) gồm 4 tác vụ *(đã chốt)*:
   - **Tìm ảnh (Lens):** Gửi vùng ảnh cắt ra trình duyệt mặc định (Google Lens / Yandex / Bing) và đóng overlay.
   - **Copy chữ (OCR):** Chạy OCR local lên vùng chọn (mặc định dùng gói OCR đã chọn, có nút chuyển nhanh gói `CJK+EN` / `Vietnamese/Latin` / `Korean` ngay trên thẻ) → hiện **thẻ kết quả nổi Slint** với văn bản bôi đen/copy được, nút *Copy tất cả* và nút *Tìm trên Google*.
   - **Dịch nhanh:** Chạy OCR local + Translator hiện tại → hiện bản dịch tiếng Việt ngay trong **thẻ kết quả nổi Slint** (kèm nút copy và đối chiếu bản gốc).
   - **Hỏi AI:** Mở ô nhập câu hỏi ngắn ngay trong **thẻ kết quả nổi Slint** (kèm các nút gợi ý nhanh như *Giải thích*, *Tóm tắt*, *Dịch tự nhiên*) gửi ảnh vùng chọn (và/hoặc chữ OCR) tới provider AI người dùng đã cấu hình (Gemini / OpenAI-compatible / Ollama) và hiển thị câu trả lời dạng streaming ngay trong thẻ. Nếu chưa cấu hình AI provider, hiển thị nút mở nhanh tới *Settings → Dịch & AI*.

#### D. Giao diện Settings (Slint) *(Architect Call — cấu trúc 5 tab)*
1. **Chung (General):** Ngôn ngữ giao diện (Tiếng Việt / English), khởi động cùng hệ thống, phím tắt toàn cục.
2. **Live Translate:** Chế độ hiển thị (`Phủ đè trực tiếp` / `Bảng dịch nổi`, kèm thông báo rõ nếu đang ở GNOME Wayland hoặc cửa sổ thả nổi trên Wayland), ngôn ngữ nguồn truyện (`CJK + EN (Unified)`, `Tiếng Nhật`, `Tiếng Hàn`, `Tiếng Trung`, `Tiếng Anh`), hướng đọc (`Tự động` / `Phải → Trái (Manga)` / `Trái → Phải`), ngôn ngữ đích (`Tiếng Việt` mặc định), độ trễ debounce (ms), độ mờ & cỡ chữ overlay.
3. **OCR & Models:**
   - Danh sách gói model PP-OCRv5 (`CJK + EN`, `Korean`, `Vietnamese / Latin`) với tuỳ chọn tier `mobile` *(mặc định)* hoặc `server`, hiển thị dung lượng, nút Tải / Xóa, thanh tiến trình, kiểm tra SHA-256 và tuỳ chọn nhập thư mục offline.
   - Ngôn ngữ OCR mặc định cho chế độ **Square to Search** (tách riêng với ngôn ngữ đọc truyện nếu người dùng hay lướt web tiếng Việt/Anh).
   - Thiết bị chạy: **`Auto / CPU / DirectML (Windows)`** *(không hiện CUDA ở v1 theo 3A)*.
   - Thời gian tự giải phóng RAM khi nhàn rỗi *(Architect Call: mặc định 5 phút)*.
4. **Dịch & AI (Translation & AI):**
   - Bộ dịch mặc định miễn phí (Google Translate web + dự phòng Bing, ghi chú minh bạch cơ chế và trạng thái).
   - Cấu hình tuỳ chọn **BYOK** (Gemini, OpenAI-compatible với Base URL tuỳ chỉnh, DeepL) và **Ollama local** (`http://localhost:11434`).
   - Cho phép chọn riêng provider cho **Dịch truyện Live** và **Hỏi AI**; hỗ trợ tuỳ chỉnh prompt dịch truyện (giữ xưng hô tự nhiên, không dịch tên riêng) khi dùng LLM.
   - Lưu API key an toàn bằng khóa bảo mật hệ điều hành (`keyring`: Windows Credential Manager / Linux Secret Service) thay vì lưu plain-text trong file cấu hình *(Architect Call)*.
5. **Tìm ảnh (Visual Search):** Chọn công cụ mặc định (`Google Lens`, `Yandex`, `Bing`) và bật/tắt fallback copy clipboard.

### 5.3. Lộ trình chia giai đoạn (Phased Milestones) *(Architect Call)*
- **Giai đoạn 1 — Nền tảng + Square to Search cốt lõi:** Dựng workspace 6 crate, System Tray, Settings cơ bản, chụp màn hình một lần (Windows + Linux X11/Wayland), manifest tải & chạy OCR local (`oar-ocr`/`ort` với CPU + DirectML, hộp thoại tải starter `mobile` lần đầu), bộ dịch web miễn phí mặc định (Google + Bing fallback), thanh công cụ nổi + thẻ kết quả nhanh (Copy chữ / Dịch nhanh) + Tìm ảnh Google Lens ra trình duyệt.
- **Giai đoạn 2 — Continuous Live Translate:** Vòng lặp chụp liên tục có frame-diff + debounce, gom cụm bóng thoại (hỗ trợ cả chữ dọc manga Nhật), LRU cache + batch translate, lớp phủ xuyên chuột không tự chụp (`WDA_EXCLUDEFROMCAPTURE` trên Windows, chụp cửa sổ đích trên Linux X11), chế độ Bảng dịch nổi, và thanh điều khiển mini Live.
- **Giai đoạn 3 — BYOK AI, Wayland & Đóng gói:** Tích hợp Gemini / OpenAI-compatible / DeepL / Ollama + luồng Hỏi AI streaming, lưu API key qua `keyring`, hoàn thiện Wayland `GlobalShortcuts` + PipeWire window stream + `wlr-layer-shell` fullscreen/maximized cho KDE/Hyprland/Sway, kiểm thử HiDPI đa màn hình và đóng gói phát hành Windows/Linux (kèm `.dll`/`.so` của ONNX Runtime, không kèm CUDA).

---

## 6. Phần để lại cho người triển khai (Left to the Builder)

- Chọn chính xác phiên bản crate và feature flag trong `Cargo.toml` (đánh giá độ tương thích build MSVC của `oar-ocr` so với bọc `ort` trực tiếp cho PP-OCRv5; lựa chọn giữa `xcap`, `windows-capture`, `ashpd` + `pipewire-rs`).
- Chọn nguồn repo/mirror cụ thể cho manifest ONNX PP-OCRv5 (ưu tiên nguồn ổn định, giấy phép mở, ghim rõ phiên bản + SHA-256).
- Thiết kế visual chi tiết (bảng màu light/dark, bo góc, spacing, hiệu ứng hover/transition) trong các file `.slint`, kiểm chứng giao diện bằng `slint-viewer --screenshot` và `slint-viewer --check`.
- Thông số kỹ thuật nội bộ: kích thước channel async Tokio, ngưỡng pixel-diff mặc định, thuật toán hình học gom bóng thoại (kèm bộ unit test trên các mẫu tọa độ manga/manhwa thực tế), và chiến lược retry/backoff của bộ dịch web miễn phí.
- Kịch bản đóng gói phát hành (Windows zip/installer, Linux AppImage/tarball kèm `.so` của ONNX Runtime).
