[English](../README.md) | Tiếng Việt

# AutoAgentFactory (`aaf`)

`aaf` là một CLI Rust cho người chạy nhiều coding agent trên một máy: một gate ký biên nhận (`aaf gate`), một hook bảo vệ đa host cho Claude Code, Codex CLI và Gemini CLI (`aaf-hook`), một hàng chờ toàn máy cho lệnh build nặng (`aaf slot`), và một vòng chạy task qua agent → gate → PR (`aaf board`, `aaf run`). Nó thay cho bộ script gate + hook viết tay theo từng repo: cùng một binary, cùng một cấu hình ba tầng cho mọi repo, và biên nhận nằm ngoài repo, có chữ ký, agent không tự ghi được.

Hợp đồng kỹ thuật: [`docs/PLAN.md`](PLAN.md). Mô hình đe doạ và các lỗ hổng đã biết: [`SECURITY.md`](../SECURITY.md). Giấy phép: Apache-2.0.

## Build và cài

Cần Rust theo `rust-toolchain.toml` (1.100.0) và `git` ≥ 2.34. Chạy được trên macOS (Seatbelt) và Linux (chưa có sandbox: `aaf run` cần `--unsandboxed`, tích hợp bubblewrap `bwrap` đang chờ; xem mục Trạng thái). Ngoài build từ mã nguồn bên dưới, v3.4 cung cấp thêm script cài đặt `scripts/install.sh` (tải tarball release có kiểm SHA-256 hoặc `--from-source`), công thức Homebrew (`packaging/homebrew/aaf.rb`) và Docker image (`packaging/docker/`).

```sh
cargo build --release -p aaf-cli -p aaf-hook
# ra: target/release/aaf và target/release/aaf-hook
```

Đưa cả hai lên `PATH`, đặt cạnh nhau:

```sh
cargo install --path crates/aaf-cli --locked
cargo install --path crates/aaf-hook --locked
# cả hai nằm ở ~/.cargo/bin
```

- `aaf init` tìm `aaf-hook` trên `PATH` trước và khi đó đăng ký bằng **tên trần** `aaf-hook` (dạng dùng được trên máy khác và trong worktree của task). Chỉ khi hook không có trên `PATH` mà nằm cạnh binary `aaf` thì nó ghi đường dẫn tuyệt đối; nếu đường dẫn đó trỏ vào `target/release/`, một lần `cargo clean` làm hook biến mất và `aaf doctor` báo `hook_binary` FAIL.
- `.claude/settings.json` phải được **commit**. Agent của `aaf run` làm việc trong worktree cắt từ commit; worktree không có đăng ký hook thì `aaf run` không khởi động agent và đưa task sang Blocked (xem "Chạy task"). `aaf init` in lệnh `git add … && git commit` khi file chưa được commit; `aaf doctor` có mục `hook_committed`.
- `aaf` phải gọi được bằng tên trần. Khi hook chặn một lệnh build, nó bảo agent chạy `aaf slot -- <lệnh>`; hook chỉ công nhận từ lệnh đúng là `aaf` (`./aaf`, `/đường/dẫn/aaf` không tính), và git hook hay CI bọc `aaf slot --` cũng cần nó trên `PATH`.

## Bắt đầu trong một repo

```sh
cd <repo>
aaf init      # dò stack, ghi .autofactory/config.toml, gộp hook vào .claude/settings.json,
              # thêm .autofactory/config.local.toml vào .gitignore
aaf doctor    # kiểm cài đặt; exit 2 nếu một mục bắt buộc FAIL
aaf gate      # lint + test_scoped, ghi biên nhận có chữ ký
aaf slot -- ./gradlew assembleDebug   # chạy một lệnh nặng qua hàng chờ toàn máy
```

`aaf init` chỉ điền sẵn `[commands]` khi chắc chắn: Android có `gradlew` và không có product flavor (kể cả khi build Gradle duy nhất nằm một thư mục dưới gốc repo: lệnh thành `cd <thư mục> && ./gradlew …`), Rust, Go, Flutter (bảo thủ: chỉ điền `flutter test` và `flutter analyze` khi `pubspec.yaml` có `sdk: flutter` VÀ repo có thư mục `test/`; gói Dart thuần rơi về `auto`), và Unity có `m_EditorVersion` cùng một assembly test EditMode (lệnh `-runTests -testPlatform EditMode` của đúng phiên bản editor ở Unity Hub, hoặc binary trong `UNITY_PATH`). Với Unity không có test EditMode, repo có nhiều build Gradle con, iOS (`.xcworkspace`/`.xcodeproj`/`Package.swift`), React Native, Expo (`app.json`/`app.config.*`/dep `expo`), Web (`package.json`), hoặc Python, lệnh để trống và gate trả `UNVERIFIED` (exit 2) cho tới khi bạn điền `test_scoped` và `test_full` vào `.autofactory/config.toml`. Chạy lại `aaf init` an toàn: config cũ được giữ, trừ khi có `--force`.

## Lệnh

Cờ chung `--json`: stdout đúng một JSON object `{ok, command, status, …, duration_ms}`. Exit chung: `0` thành công, `1` lỗi runtime/config/I/O, `2` bị chặn, chưa kiểm được, bị từ chối theo policy, hoặc sai cú pháp.

| Lệnh | Việc | Cờ chính | Exit đáng nhớ |
|---|---|---|---|
| `aaf init` | Dò stack, sinh config tầng 2 (gồm `[proof]`, `[github]`), đăng ký hook, sửa `.gitignore`, tuỳ chọn đồng bộ skill | `--force` ghi đè config, `--with-skills`, `--host <claude\|agents\|all>`, **`v3.4`** `--hook-host <claude\|codex\|gemini\|all>` | `2` khi có `AAF_RUN_ID`; `1` không thấy `aaf-hook` hoặc `settings.json` hỏng (file giữ nguyên) |
| `aaf doctor` | Kiểm git, repo, config, hook, data dir, quyền `receipt.key`; cảnh báo về hook chưa commit (`hook_committed`), `claude`, sandbox, `keychain_sandbox_exposure` (trên macOS), account, Gradle daemon đang sống, git hook gọi build không qua hàng chờ, và các mục advisory `skills_sync`, `proof_readiness`, `github_board`. Chỉ đọc. Ngoài repo git nó là doctor của máy: các mục của repo in `n/a` (`applicable: false` trong JSON) và không tính | | `2` khi mục bắt buộc FAIL; mục `warn` và `n/a` không đổi exit |
| `aaf status` | Thống kê chỉ đọc phạm vi (`scope`), task (đủ 8 trạng thái + `blocked_reasons`), run, token/chi phí USD, trạng thái account (gồm `leased`), và khoá throttle đang hoạt động (`active_slots`, `draining`) | `--all` (mọi repo) | `0` |
| `aaf watch` | Dashboard terminal 100% chỉ đọc (0 cổng mạng, không giữ khoá độc quyền, không tạo `receipt.key` hay file `-shm`/`-wal` trong thư mục chỉ đọc) hiển thị trực tiếp 4 bảng: task theo trạng thái + `unblock_cycles`/`blocked_reason`, khe nặng (`free`/`busy`/`orphan`/`stale`, `drain`, áp lực RAM, số lệnh đã đo), pool account (`selection_mode`, `pinned_account`, cooldown, `tokens_5h`) và biên nhận ký gần nhất | `--repo <path>`, `--all`, `--interval <SECS>` (mặc định 2), `--once` | `0` |
| `aaf gate` | Chạy gate (và xác minh proof khi `[proof].required = true`), ghi biên nhận. JSON có `reused_receipt: true` khi kết quả là biên nhận `CLEAR` cũ được trả lại và không lệnh nào chạy | `--scoped` (mặc định), `--full`, `--release` (chọn một) | `0` `CLEAR`; `2` `BLOCKED`/`UNVERIFIED`; `1` gate không chạy được |
| `aaf proof record\|verify\|show` | Kiểm tra PNG, `.adb-denylist`, `allowed_tools` và ký HMAC proof gắn với `tree_fp` | `record`: `--image`, `--tool`, `--device`, `--task-id` | `2` khi `verify` ra `BLOCKED`, gọi `record` trong `AAF_RUN_ID`, hoặc `record` gặp ảnh/tool/device vi phạm policy; `1` khi lỗi runtime/I/O |
| `aaf plan "<goal>"` | Tạo prompt lập kế hoạch chỉ đọc có khung `<untrusted-data>` và chặn path traversal | `--file <path>` (lặp được), `--dry-run` | `2` khi goal/path/file không hợp lệ hoặc bị từ chối |
| `aaf skills list\|sync` | Liệt kê hoặc đồng bộ gói 15 `SKILL.md` theo profile mà không ghi đè chỉnh sửa của người dùng | `list`: `--profile`; `sync`: `--host <claude\|agents\|all>`, `--dir`, `--profile`, `--all`, `--only`, `--force` | `2` khi có `AAF_RUN_ID`; `1` profile/config lỗi hoặc gặp symlink |
| `aaf slot -- <cmd> [args…]` | Chạy lệnh bất kỳ qua hàng chờ lệnh nặng; stdout/stderr là của lệnh. Chạy được ngoài repo | `--label <NAME>`, `--timeout <SECS>` | exit của lệnh; `128+N` nếu bị tín hiệu N giết; `75` hết `--timeout` khi còn chờ (lệnh chưa chạy); `127`/`126` không khởi động được; `2` thiếu `--` hoặc có `--json` |
| `aaf approve --reviewer <tên>` | Ký approval cho `lock_fp` hiện tại (việc của người) | | `2` khi stdin không phải TTY, có `AAF_RUN_ID`, hoặc không có file khoá nào đổi |
| `aaf accounts list` / `next` / `cooldown <ID>` | Liệt kê account, account kế tiếp, đặt/xoá cooldown | `cooldown`: `--until <unix giây>`, `--clear` (mặc định 5 giờ) | `2` khi `next` không còn account dùng được |
| `aaf board add` | Thêm task Ready cho repo hiện tại | `--title`, `--spec`, `--accept` (lặp được), `--priority P0..P3` (mặc định P2), `--blocked-by <id>`, **`v3.2`** `--scope <prefix>` (lặp được) | |
| `aaf board list` / `show <ID>` / `trace <ID>` | Xem task hoặc dòng thời gian run + event (kèm `events_truncated`) | `list`: `--state`, `--all`; `trace`: `--limit <N>` | |
| `aaf board rework <ID>` | Đưa task `review` quay lại `gating` (hoặc `ready` khi có `--rerun`) | `--reason "..."`, `--rerun` | `2` khi có `AAF_RUN_ID` hoặc task không ở `review` |
| `aaf board split <ID>` | Chủ động tách task gốc (`Ready`/`Backlog`) thành 2..=4 subtask song song có `owned_paths` rời nhau và nối DAG (planner chỉ đọc trong sandbox) | `--max <2..=4>` (mặc định 4), `--dry-run`, `--unsandboxed` | `2` khi có `AAF_RUN_ID`, task là subtask (`depth > 0`), không ở `Ready`/`Backlog`, hoặc thiếu `--unsandboxed` ngoài macOS |
| `aaf board import-issues` / `sync-github` | Nhập issue theo allowlist + khử trùng 72h (gọi `gh` qua `aaf_fp::tools`); đồng bộ trạng thái lên GitHub Projects v2 hoặc nhãn `status:*` | `import-issues`: `--label`, `--dry-run`; `sync-github`: `--dry-run` | `2` khi có `AAF_RUN_ID`, allowlist rỗng, `[github].enabled = false` (không `--dry-run`), hoặc `sync-github` có lỗi một phần (`partial_error`) |
| `aaf board unblock <ID>` / `unfreeze` | Đưa task Blocked về Ready; gỡ đóng băng factory (việc của người) | | `2` khi có `AAF_RUN_ID`, hoặc task không ở Blocked |
| `aaf run` | Chạy task Ready: worktree → agent → gate → PR hoặc merge queue | `--auto` (lặp tới khi ngắt), `--auto-merge`, `--auto-split`, `--max-parallel <N\|auto>`, `--unsandboxed` | `0` khi không task nào bị chặn (kể cả khi không mở được PR và task nằm ở Review); `2` khi có task kết thúc ở Blocked (`status: "blocked"`), lượt chạy dừng vì agent lỗi đăng nhập hay mạng (`status: "halted"`), factory đóng băng, đã có `aaf run` khác trên repo, có `AAF_RUN_ID`, hoặc không có sandbox mà thiếu `--unsandboxed` |
| **`v3.2`** `aaf board dep add\|rm` | Thêm (kiểm tra chống vòng) hoặc xoá cạnh phụ thuộc DAG | `<task> --blocked-by <blocker>` | `2` khi tạo vòng lặp hoặc tham số không hợp lệ |
| **`v3.2`** `aaf board graph` | In thứ tự topo, đường găng (`critical path`) và danh sách task `cascade-blocked` | `--all` | `0` |
| **`v3.2`** `aaf board approve-plan <ID>` | Duyệt kế hoạch sau `plan_stage` và đưa task về Ready (việc của người) | | `2` khi có `AAF_RUN_ID` hoặc task không chờ duyệt plan |
| **`v3.2`** `aaf board undo <ID>` | Lùi worktree của task về snapshot đã xác minh `refs/aaf/snapshots/*` gần nhất (việc của người) | `--dry-run` | `2` khi có `AAF_RUN_ID` hoặc không có snapshot hợp lệ |
| **`v3.2`** `aaf board steer <ID>` | Nối ghi chú `steer:` đã lọc mà không sửa `ImmutableSpec` (việc của người) | `--note "..."` | `2` khi có `AAF_RUN_ID` hoặc note trống |
| **`v3.2`** `aaf gc` | Dọn worktree mồ côi, ref/snapshot của task đã xong, và file stream/report hết hạn | `--dry-run` | `0` |
| **`v3.2`** `aaf handover <ID>` | Xuất báo cáo bàn giao chỉ đọc (`md`, HTML có CSP kèm PNG đã xác minh `<= 4 MiB`, hoặc `json`) | `--format <md\|html\|json>`, `--out <path>`, `--open` (TTY + macOS) | `2` khi vi phạm policy `--out`/`--open` hoặc có `AAF_RUN_ID` |
| **`v3.2`** `aaf cockpit` | Cockpit TTY tương tác đồng bộ (0 cổng mạng, 0 crate mới, lọc ANSI/OSC, chỉ cho người dùng TTY) | `--repo <path>`, `--all` | `2` khi không phải TTY hoặc có `AAF_RUN_ID` |
| **`v3.4`** `aaf telegram` | Companion Telegram outbound 0 cổng mạng để xem trạng thái, handover và duyệt 2 bước `/confirm` (việc của người) | `--once`, `--all`, `--repo <path>`, `--send-test` | `2` khi có `AAF_RUN_ID`, chưa bật `[telegram]` tầng 1, hoặc `allowed_chat_ids` rỗng |

`aaf-hook` không phải lệnh gõ tay: agent host tự chạy nó (hỗ trợ `--host <auto|claude|codex|gemini>`, mặc định `auto`), đọc JSON `PreToolUse` (Claude Code / Codex CLI) hoặc `BeforeTool` (Gemini CLI) trên stdin, exit `0` (cho qua) hoặc `2` (chặn).

## Chạy task (`aaf run`)

1. **Hook là điều kiện**: với executor `claude`, trước khi khởi động agent `aaf run` kiểm `.claude/settings.json` trong worktree của task có đăng ký `aaf-hook` cho PreToolUse với matcher phủ `Bash`, `Write`, `Edit`, `MultiEdit`, `Read`. Thiếu thì agent không chạy, task sang Blocked với lý do nêu cách sửa: commit `.claude/settings.json` trên nhánh base, rồi `aaf board unblock <id>`.
2. **Môi trường của agent** là danh sách cho phép, không phải môi trường của shell đã gõ `aaf run`: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LC_*`, `TERM`, `TMPDIR`, `JAVA_HOME`, `ANDROID_HOME`, `ANDROID_SDK_ROOT`, `GRADLE_USER_HOME`, `CARGO_HOME`, `RUSTUP_HOME`, `UNITY_PATH`, `DEVELOPER_DIR`, proxy và chứng chỉ (`HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` và dạng chữ thường, `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `SSL_CERT_DIR`), cộng `AAF_RUN_ID`, `CLAUDE_CONFIG_DIR` và `AAF_DATA_DIR` do `aaf` đặt. `GITHUB_TOKEN`, `GH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_*` và mọi biến khác không tới agent. Cần thêm biến nào thì nêu tên trong `[run] env_pass = ["TÊN"]` ở `~/.config/autofactory/config.toml` (tầng 1, cho mọi repo trên máy) hoặc `.autofactory/config.toml` (tầng 2); hai danh sách được cộng lại. Tên dạng credential (`*_TOKEN`, `*_KEY`, `*_SECRET`, `*PASSWORD*`) và `NODE_OPTIONS` chỉ được nêu ở tầng 1 (`~/.config/autofactory/config.toml`); nêu ở tầng 2 thì bị từ chối. `AAF_*`, `PATH`, `HOME`, `GIT_*` đổi hướng git, `DYLD_*`/`LD_*` và các biến nạp code (`JAVA_TOOL_OPTIONS`, `PYTHONPATH`, `PERL5OPT`, `RUBYOPT`, `NODE_PATH`…; danh sách đủ ở `SECURITY.md` mục 5) bị từ chối ở mọi tầng, kể cả tầng 1; `GRADLE_OPTS`, `JAVA_OPTS` được phép. Tên bị từ chối làm `aaf run` dừng trước khi lấy task và nêu tên sai. `gh pr create` do `aaf` chạy, vẫn dùng đăng nhập `gh` của bạn.
3. **Model**: `[run] model = "…"` và `effort = "…"` thành `claude --model` / `--effort`. Để trống thì không truyền cờ. File cục bộ (tầng 3) ghi đè được hai key này.
4. **Account**: `[accounts] selection` trong config tầng 1 (`~/.config/autofactory/config.toml`) chọn một trong ba chế độ; không ghi gì là `pinned`.
   - `pinned` (mặc định): mỗi repo gắn với một account. Account lấy từ `account` trong `.claude/settings.local.json` của repo (khớp id hoặc tên account, kể cả tên đặt bằng `accounts.aliases`); không có thì là account repo chạy gần nhất; repo chưa chạy lần nào thì lấy account rảnh ít dùng nhất và giữ luôn account đó. Account đã gắn chạm limit hoặc đang bận thì task chờ, không chuyển sang account khác cùng provider; `aaf run` in account đang chờ và thời điểm hết cooldown.
   - `sticky`: task và repo giữ account đã dùng, nhưng account đó chạm limit hoặc bận thì chuyển sang account rảnh khác và giữ account mới.
   - `least_used`: mỗi lượt lấy account không cooldown, lease rảnh, ít token nhất trong 5 giờ; chạm limit thì task chạy tiếp trên account khác còn rảnh.

   `aaf accounts next --json` và `aaf doctor --json` có `selection` và `binding` (account đã gắn, nguồn, cooldown). Ở `pinned`, cross-audit chạy trên chính account của repo và ghi `same_account: true`.
5. **Sandbox** (macOS Seatbelt; trên Linux `aaf run` chưa dùng bubblewrap `bwrap` và cần `--unsandboxed`): xem `SECURITY.md` mục 5 cho danh sách ghi được và bị đóng.

## Cấu hình ba tầng

| Tầng | File | Chứa |
|---|---|---|
| 1, máy | `~/.config/autofactory/config.toml` (thư mục đổi bằng `AAF_CONFIG_DIR`) | `[machine]`, `[accounts]`, `[safety]`, `[notifications]`, `[cross_audit]`, `[run]` chỉ với `env_pass` |
| 2, repo, commit | `.autofactory/config.toml` | `[project]`, `[commands]`, `[gate]`, `[unblocker]`, `[run]`, `[cross_audit]`, `[proof]`, `[github]`, `[github.columns]` |
| 3, cục bộ, gitignore | `.autofactory/config.local.toml` | chỉ được siết `[gate]` và `[cross_audit]` (`require_different_provider`, `audit_scope`, `score_gate`, `min_average_score`, `min_criterion_score`) |

Thứ tự: tầng 1 < tầng 2 < tầng 3 < env `AAF_*` < cờ CLI. Từ tầng 3 trở lên chỉ siết được gate: thêm `head_lock_globs`, nâng `store_compliance` (`off < auto < always`), tăng `min_android_target_sdk`. `[commands]`, `project.profile`, `run.executor`, `run.executor_cmd` chỉ đọc ở tầng 2, `run.env_pass` ở tầng 1 và 2; đặt ở tầng cao hơn thì bị bỏ qua kèm cảnh báo. `run.model`, `run.effort`, `run.max_parallel`, `run.run_timeout_secs` lấy theo tầng cao hơn. Key lạ ở bất kỳ tầng nào là lỗi load (exit 1). `commands.repro` vẫn chỉ là schema + API thư viện chạy thủ công (`ReproCommand::argv`), **không bao giờ tự thực thi** trong gate hay factory.

Biến môi trường: `AAF_DATA_DIR` (mặc định `~/.local/share/autofactory`), `AAF_CONFIG_DIR`, `AAF_WORKTREE_DIR` (mặc định `~/.cache/autofactory/worktrees`), `AAF_STORE_COMPLIANCE`, `AAF_MIN_ANDROID_TARGET_SDK`, `AAF_MAX_PARALLEL`, `AAF_AUTO_MERGE` (chỉ tắt được). `AAF_RUN_ID` và `AAF_HEAVY_SLOT` do `aaf` tự đặt, không gán tay.

## Chống quá tải máy

1. **Khe nặng**: `commands.build`, `test_scoped`, `test_full` của gate, `commands.lint` khi chuỗi lệnh của nó gọi công cụ build (`./gradlew lint`, `xcodebuild …`; lint khác chạy ngay), và mọi `aaf slot -- <cmd>` dùng chung khe toàn máy (`throttle/heavy.<n>.lock` trong data dir). Agent trong sandbox của `aaf run` cũng xếp hàng ở đó: sandbox mở riêng thư mục `throttle/` (chỉ đọc và khoá) và `throttle/guest/` (ghi), phần còn lại của data dir vẫn đóng. `aaf gate` thì không chạy được trong sandbox (không tới được khoá ký); gate là việc của `aaf run`, chạy sau lượt của agent. Trần là `machine.max_heavy_parallel`; `0` = số CPU / 4, ít nhất 1. Khe thứ hai trở đi chỉ nhận lệnh đã có số đo RAM đỉnh và khi RAM trống đủ 1,25 lần số đó; lệnh chưa đo chạy một mình. Lần đọc 0 byte không tính là số đo, nên lệnh xong quá nhanh để đo được vẫn là "chưa đo" và vẫn chạy một mình.
2. **Độc quyền**: lệnh gọi `gradle`/`gradlew`, `xcodebuild`, `Unity`/`unity-editor`, hoặc có `-batchmode`/`-runTests`/`-executeMethod`, luôn chạy một mình, kể cả `gradle --version`.
3. **Tắt daemon**: lệnh nặng nhận thêm `GRADLE_OPTS=-Dorg.gradle.daemon=false -Dorg.gradle.project.kotlin.compiler.execution.strategy=in-process`. Daemon còn sống sau lệnh thì có cảnh báo, không bị giết.
4. **Tải và bộ nhớ**: áp lực bộ nhớ khác Normal thì chờ; tải CPU vượt `max_load_percent` (150) VÀ CPU bận thật ≥ `max_cpu_percent` (85; 0 hoặc không đo được thì chỉ xét tải) thì chờ tối đa `heavy_wait_secs` (1800) rồi vẫn chạy. Lệnh chạy ở `nice -n 10` (macOS thêm `taskpolicy -b`). Bộ nhớ lên Critical thì lệnh ở khe muộn nhất bị `SIGSTOP`, về Normal thì `SIGCONT`; khe 0 không bao giờ bị dừng.
5. **`aaf` bị giết**: Ctrl-C ở terminal tới lệnh nặng qua relay. `kill <pid>` gửi riêng cho `aaf slot`/`aaf gate` (TERM, INT, HUP, kể cả KILL) cũng kết thúc lệnh nặng: relay thấy `aaf` chết, gửi TERM cho group của lệnh, sau 10 giây thì KILL. Lệnh chỉ chạy tiếp khi relay cũng chết (SIGKILL cả process group của `aaf`); khi đó khe vẫn tính là bận cho tới khi lệnh kết thúc (file khoá còn ghi process group và thời điểm bắt đầu của lệnh), lệnh đang chờ in ra `process group <pgid> is recorded as the command of a killed aaf and still has processes` kèm cách xem group đó trước khi tự kết thúc nó, và không gì bị giết tự động.
6. **Lồng nhau**: lệnh do `aaf` khởi chạy mang `AAF_HEAVY_SLOT=<slot>:<token>`. `aaf slot` bên dưới nó chạy ngay, không xếp hàng, chỉ khi khe đó đang có người giữ và token khớp file token của khe (`locks/heavy.<n>.token`, agent trong sandbox không đọc được). Giá trị tự gán (`AAF_HEAVY_SLOT=0`) không có tác dụng, và hook chặn agent gán biến này.

Bọc git hook hoặc bước CI:

```sh
# .git/hooks/pre-commit
aaf slot --label pre-commit-lint -- ./gradlew :app:lintDebug --no-daemon
```

```yaml
# bước của self-hosted runner trên cùng máy
- run: aaf slot --timeout 3600 -- ./gradlew testDebugUnitTest
```

Hết `--timeout` thì exit 75 và lệnh chưa chạy. Bước `git_hooks_heavy` của `aaf doctor` chỉ ra `file:dòng` trong `pre-commit`/`pre-push` còn gọi build trần.

## Biên nhận và approval

1. Mỗi `aaf gate` ghi `receipts/<repo_id>/latest.json` trong data dir, ký HMAC-SHA256 bằng `receipt.key`. Không file nào nằm trong repo. Data dir và các thư mục con ở 0700, file ở 0600.
2. Biên nhận ràng buộc: `repo_root` (canonical), `tree_fp` (`git-tree:<hash>` của cả file tracked lẫn untracked không bị ignore, không đổi khi commit cùng nội dung), `base_sha`, `config_hash`, mode, status. Tính `tree_fp` không ghi object nào vào `.git/objects` của repo và chạy được trong sparse checkout (cần git ≥ 2.34).
3. Gate chạy lại trên cùng cây, cùng base, cùng config với mode không cao hơn thì trả lại biên nhận `CLEAR` cũ, không chạy lệnh nào. Cây đổi trong lúc gate chạy thì kết quả là `UNVERIFIED`.
4. Head lock: file khớp `gate.head_lock_globs` (test, CI, `Cargo.toml`, `build.gradle`, `.adb-denylist`, `.adb-allowlist`…) bị sửa, xoá hoặc đổi tên thì gate `BLOCKED` cho tới khi có approval ký cho đúng `lock_fp` đó. File mới thêm được miễn, trừ `.claude/**`, `CLAUDE.md`, `.mcp.json`, `.agents/**`, `AGENTS.md` (khi danh sách glob của repo có chúng): vì vậy `aaf gate` ngay sau `aaf init`, khi `.claude/settings.json` chưa commit, ra `BLOCKED` cho tới khi bạn commit file đó hoặc `aaf approve`.
5. `aaf approve` là việc của người, chạy ở terminal: CLI từ chối khi stdin không phải TTY hoặc có `AAF_RUN_ID`, và hook chặn agent gọi `aaf approve`, `aaf board unblock`, `aaf board unfreeze`. Submit store (`fastlane deliver`, `eas submit`, `gradlew publish*`…) chỉ qua hook khi có biên nhận `--release` `CLEAR` khớp cây hiện tại.
6. File nằm trên đĩa mà biên nhận không ký: file tracked mang cờ `skip-worktree`/`assume-unchanged` luôn làm gate `BLOCKED` (lý do nêu lệnh gỡ cờ) và làm hook từ chối submit store. File chỉ bị ignore bởi `.git/info/exclude`, excludes toàn cục hay một `.gitignore` untracked tự ignore chính nó: `aaf gate --scoped`/`--full` chỉ in `warning: work tree: …`; `aaf gate --release` ra `BLOCKED`; gate của factory xoá chúng trong worktree của task. File bị ignore bởi `.gitignore` đã commit (`target/`, `node_modules/`…) không bao giờ bị xoá và không bao giờ được ký. Chi tiết: `SECURITY.md` mục 9.

## Gate của chính repo này

Repo này chưa chạy `aaf init` cho chính nó. Gate là ba lệnh, cũng là CI (`.github/workflows/ci.yml`):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Lượt chạy dùng làm bằng chứng đặt `AAF_REQUIRE_REAL=1` cho `cargo test`: các test `aaf-cli` chạy lệnh nặng qua binary thật (`cli_gate`, `cli_gate_orphan`, `cli_slot`, `cli_slot_nested`, `cli_slot_signal`) bình thường tự bỏ qua khi áp lực bộ nhớ khác Normal (dòng `SKIPPED …` trên stderr, test vẫn báo `ok`); với biến này chúng thất bại.

## Gỡ rối: build trong sandbox của `aaf run`

Những chỗ một build thật có thể vấp với luật sandbox hiện tại (`SECURITY.md` mục 5). Chưa có build Gradle, cargo, Unity hay Xcode thật nào chạy bên trong profile này; danh sách dưới đây suy từ code và từ test với script thế chỗ agent.

- **Cài hay cập nhật công cụ nằm trên `PATH`** bị từ chối trong sandbox: `platform-tools` của Android SDK luôn bị đóng; mọi thư mục trong `PATH` của `aaf run` nằm trong chỗ ghi được (cache build, thư mục tạm, Android SDK, worktree của task) cũng bị đóng, ví dụ `emulator`, `cmdline-tools/latest/bin` của SDK khi chúng có trong `PATH`. Vì vậy `sdkmanager` cập nhật các gói đó, `cargo install` (`~/.cargo/bin`), `dart pub global activate` (`~/.pub-cache/bin`), `yarn global add` khi thư mục bin toàn cục của nó nằm trong cache và có trong `PATH`, đều thất bại. Cài hay cập nhật chúng ngoài agent.
- **Thư mục gốc của cache bị khoá entry**: trên chính `~/.gradle`, `~/.cargo`, `~/.android`, `~/.m2`, `~/.pub-cache`, thư mục SDK, config dir của account và worktree của task, `touch`, `chmod`, `xattr -w`, `mv`, `rm` bị từ chối; tạo, đổi tên, xoá file bên trong vẫn được.
- **Thư mục cache rỗng xuất hiện**: lượt chạy có sandbox đầu tiên tạo `~/.cargo`, `~/.gradle`, `~/.android`, `~/.m2`, `~/.pub-cache` (và thư mục SDK, config dir của account) nếu chúng chưa có.
- **Mọi task bị Blocked với lỗi `PATH entry …`**: một entry tuyệt đối trong `PATH` của `aaf run` có dấu `"`, ký tự điều khiển, không phải UTF-8, hay có `..` sau một thành phần không tồn tại, làm việc dựng profile thất bại ở mọi lần khởi động agent. Sửa `PATH` trước khi chạy `aaf run`.
- **`~/.android/debug.keystore` chưa có**: file bị cấm ghi trong sandbox, nên chạy một build debug ngoài sandbox một lần để Android Gradle Plugin tạo nó.
- **`GRADLE_USER_HOME`, `CARGO_HOME`, `RUSTUP_HOME` trỏ ra ngoài vị trí mặc định** trong home: agent nhận biến nhưng thư mục đó không ghi được.
- **Agent không ra được mạng sau proxy có login**: URL proxy có `user:password@` không được chuyển cho agent; log của lượt chạy có dòng `[aaf] HTTPS_PROXY was not handed to the agent …`. Nêu tên biến đó trong `run.env_pass` nếu chấp nhận đưa login cho agent.

## Gỡ rối: `aaf run` dừng hoặc từ chối

- **`status: "halted"`, exit 2, dòng `stopped: <task>: the agent CLI failed: …`**: agent `claude` không làm việc được vì đăng nhập (mã lỗi account như `authentication_failed`, hoặc HTTP 401/403) hay vì mạng. Task đó đã về `Ready` và không bị tính là một lần thử; `aaf run` không khởi động task nào nữa. Đăng nhập lại account được nêu (hoặc sửa mạng, proxy, DNS) rồi chạy lại `aaf run`. Có nhiều account thì lượt chạy vẫn dừng, không tự đổi sang account khác. Executor khác `claude`, lỗi 5xx của provider và proposer subtask không được nhận diện kiểu này: lỗi đăng nhập hay mạng của chúng vẫn tính như lỗi của task.
- **`run.env_pass: "<TÊN>" cannot be handed to the agent: …`**: lý do kết thúc bằng `only the machine config (~/.config/autofactory/config.toml) may name it` nghĩa là tên dạng credential hay `NODE_OPTIONS` nằm trong `.autofactory/config.toml`; chuyển nó sang `[run] env_pass` của `~/.config/autofactory/config.toml`. Lý do khác (biến nạp code, biến đổi hướng git, `AAF_*`…) nghĩa là không config nào chuyển được tên đó: bỏ nó.
- **`status: "refused"` vì `.claude/settings.json` trên base**: ngoài các lỗi đăng ký đã có, entry `aaf-hook` có `"timeout"` dưới 30 giây, hay `"env"` đặt một `GIT_*` khác `GIT_AUTHOR_*`/`GIT_COMMITTER_*`, đều làm lượt chạy bị từ chối. Bỏ `timeout` (như `aaf init` ghi) hoặc đặt ≥ 30, bỏ biến đó khỏi `env`, commit trên base.
- **`status: "refused"` với `.claude/settings.local.json on the base branch could not be read`**: git không trả lời được việc file đó có trên base hay không (hoặc có mà không đọc được); `aaf run` từ chối thay vì bỏ qua việc kiểm. Kiểm `git ls-tree <base> -- .claude/settings.local.json` trong repo rồi chạy lại.

## Trạng thái

Tại nhánh `lane/w3-fix` (trên main `31aad9e`, 2026-11-22). Hai nguồn, ghi rõ ở từng dòng: "tự chạy" là chạy khi viết tài liệu này, trên macOS, binary debug; "Wave 3" là lượt chạy thật của các lane nghiệm thu Wave 3 trên máy này (theo báo cáo của các lane đó, không chạy lại ở đây, và chạy TRƯỚC các thay đổi của `lane/w3-fix`).

| Mục (PLAN §E) | Trạng thái |
|---|---|
| E.1 fmt, clippy, test | Tự chạy: fmt exit 0, clippy exit 0, `cargo test --workspace` hai lượt đều exit 0 với 1016 passed, 0 failed, 5 ignored |
| `aaf slot` | Tự chạy (ở `43bbd10`) với lệnh thật không phải build: exit truyền qua (7), tín hiệu (143), `127`, `75` khi khe bận, `2` khi thiếu `--` hoặc có `--json`; lệnh con nhận `GRADLE_OPTS` tắt daemon |
| E.2 hook trong phiên Claude Code thật | Wave 3: đã chạy thật, hook chặn lệnh trong một phiên Claude Code thật |
| E.3 `aaf gate` trên các repo app thật | Wave 3: đã chạy trên các repo app của chủ máy; một lượt test đơn vị Gradle thật đã đi qua hàng chờ lệnh nặng |
| E.4 `aaf accounts` | Wave 3: đã chạy thật, thấy các profile trên máy |
| E.5 `aaf doctor` | Wave 3: đã chạy thật, trong repo và ngoài repo |
| E.6 một task thật qua `aaf run` | Wave 3: một task thật với agent thật đã đi tới Review trên một repo có remote không phải GitHub (không mở PR, nhánh + biên nhận) |

Thay đổi của `lane/w3-fix` mới có test, chưa có lượt chạy thật nào sau khi sửa: kiểm đăng ký hook trước khi khởi động agent, danh sách môi trường cho phép của agent, `run.model`/`run.effort`, exit 2 khi có task Blocked, danh sách ghi được và cấm ghi mới của sandbox (thử bằng `sandbox-exec` thật với script thế chỗ agent, chưa với Claude Code thật), `tree_fp` trong sparse checkout và không ghi object.

Chưa nghiệm thu thật, mới có test trên fixture hoặc đồ giả:

- Build Unity và xcodebuild thật qua hàng chờ; build thật bất kỳ chạy bên trong sandbox Seatbelt; `SIGSTOP`/`SIGCONT` trên build thật.
- Đường GitHub: `gh pr create` tới một remote GitHub, `gh issue list`; webhook thông báo.
- Chuyển account khi chạm limit THẬT (mới có fixture tổng hợp); nhận diện chạm limit từ output thật.
- `aaf run --auto`, gom lô gate nhiều task, và cross-audit với agent thật.
- Linux: `aaf run` chưa bọc agent bằng bubblewrap (`bwrap`) (đang chờ tích hợp) nên luôn từ chối (exit 2) cho tới khi truyền `--unsandboxed`, kể cả khi có `bwrap` hoặc đang trong container cứng hoá (`packaging/docker/`); `aaf doctor` chỉ báo `bwrap`/container như thông tin. Việc chạy thật `bwrap`/Docker và đường đo tải qua `/proc` trên máy Linux thật vẫn cần người vận hành xác minh thực địa.

Chưa có (đừng tìm): adapter hook riêng cho agent ngoài Claude Code, Codex CLI và Gemini CLI (Antigravity, OpenCode chưa có adapter hook); lệnh gate mặc định cho iOS `.xcodeproj`, build Flutter, dự án Python không có `pyproject.toml`, và Unity không có test EditMode. Web, React Native và Expo lấy lệnh từ script trong `package.json`: test watch-mode như `jest --watchAll` của Expo được đổi thành `npx jest --ci`, còn dự án khai TypeScript có `npx tsc --noEmit` làm lint. `pyproject.toml` có pytest, ruff, mypy, chạy qua `uv`/`poetry`/`pdm` nếu có lockfile. Không có gì tự chạy `commands.repro` (chủ ý chỉ chạy tay).

Chạy không người trực (24/7): `scripts/aaf-supervise.sh <repo>` giữ cho factory chạy ở chế độ auto. Nó khởi động lại sau crash, halt hay khởi động lại máy, có back-off. Khi factory frozen thì nó chờ `aaf board unfreeze` thay vì restart lặp, và dùng `caffeinate` chống ngủ máy trên macOS. Mẫu tự khởi động nằm ở `packaging/launchd/` và `packaging/systemd/`. `aaf notify` đẩy webhook hoặc Telegram khi factory frozen, khi task bị Blocked và khi task chờ Review.

## Bổ sung hệ sinh thái v3 (`lane/v3-ecosystem`)

Các lệnh và cấu hình mới đã có unit/integration test trên fixture, chưa chạy live với GitHub API (`gh project`) hay `mobile-mcp` thật:

- **Tách lỗi hạ tầng & rework (`aaf board rework`)**:
  - `AgentOutcome.infra: Option<InfraFailure>` + `BlockedReason::Infra`: phân biệt lỗi auth/network/hạ tầng (retry T2 một lần hoặc dừng run an toàn, leo thẳng T5 nếu lặp lại) khỏi lỗi của task; không tăng bộ đếm đóng băng factory (`factory_frozen`).
  - `aaf board rework <id> --reason "..." [--rerun]`: gửi task ở trạng thái `review` quay lại `gating` (hoặc `ready` khi có `--rerun`), đính ghi chú `rework:` và phát sự kiện `rework_requested` (bị từ chối bên trong phiên agent).
- **Telemetry, dò lock thực & chế độ lập kế hoạch chỉ đọc (`aaf status`, `aaf board trace`, `aaf plan`)**:
  - `aaf status [--all]`: gộp run/token bằng một truy vấn SQL (`GROUP BY status`), báo rõ `scope`, đủ 8 trạng thái task (`backlog`..`cancelled`), phân bổ `blocked_reasons`, và dùng `try_lock_shared()` đọc thực tế trạng thái khoá sống trên `throttle/heavy.*.lock` (`active_slots`/`throttle_slots` + `slot_lock_files`), `throttle/drain.lock` (`draining`/`drain_lock_present`), và `leases/<account_id>.lock` (`leased`).
  - `aaf board trace <id> [--limit N]`: đảo đúng thứ tự thời gian từ DB (`id DESC` → `ASC`), ưu tiên `"run"` trước `"event"` khi trùng mốc giây, tính `wall_secs` an toàn bằng `saturating_sub`, và báo `events_truncated` khi vượt `--limit`.
  - `aaf plan "<goal>" [--file path] [--dry-run]`: tạo prompt lập kế hoạch chỉ đọc có khung `<untrusted-data>` và chặn path traversal.
- **Xác minh ảnh proof di động (`aaf proof record|verify|show` + `aaf gate`)**:
  - Chuẩn hóa dùng **duy nhất `mobile-mcp`** (`@mobilenext/mobile-mcp` với `MOBILEMCP_DISABLE_TELEMETRY=1`, tool `mobile-mcp:mobile_save_screenshot`) cho cả Android & iOS; ảnh PNG được kiểm tra bằng Rust thuần (magic/`IHDR`/`IDAT`/`IEND`, `min_bytes`, kích thước tối thiểu, `.adb-denylist` gồm cả tên AVD lẫn serial, `allowed_tools = ["mobile-mcp:mobile_save_screenshot"]`) và ký HMAC với domain `"proof\0"` gắn chặt `repo_root` + `tree_fp`.
  - Cấu hình tier-2 `[proof]` được `aaf init` sinh sẵn trong `.autofactory/config.toml`; khi `[proof].required = true`, `aaf gate` trực tiếp xác minh biên bản proof khớp `tree_fp` hiện tại, tự chặn gate và vô hiệu hóa tái sử dụng biên nhận cũ nếu thiếu hoặc bị sửa ảnh; `.adb-denylist` và `.adb-allowlist` nằm trong mặc định `gate.head_lock_globs`.
- **Luồng GitHub Board (`aaf board import-issues`, `aaf board sync-github`)**:
  - `aaf board import-issues [--label <L>] [--dry-run]`: fail-closed nếu `safety.owner_github_allowlist` rỗng; gọi `gh` đã phân giải ngoài repo qua `aaf_fp::tools`; bọc nội dung issue bằng `<<<UNTRUSTED_DATA_BEGIN id=…>>>` / `<<<UNTRUSTED_DATA_END id=…>>>` kèm id băm nội dung; khử trùng theo task đang mở và cửa sổ 72 giờ trên `FingerprintStore`. `--dry-run` chỉ đọc, không tạo `aaf.db`.
  - `aaf board sync-github [--dry-run]`: khử trùng mỗi issue về đúng một task thẩm quyền (trạng thái chưa kết thúc thắng `Done`/`Cancelled`, hoà phá theo `updated_at` rồi `created_at`) và ánh xạ 8 `TaskState` sang cột Super-Board (`[github.columns]`, được `aaf init` sinh sẵn); nếu cấu hình `[github].project_number` (+ tùy chọn `project_owner`) thì đồng bộ cột `Status` thật trên GitHub Projects v2 qua `gh project view|field-list|item-list|item-add|item-edit` (`mode="project_v2"`), nếu bỏ trống thì đọc nhãn hiện tại (`gh issue view --json labels`) rồi chỉ thêm/gỡ nhãn `status:*` cần thiết (`mode="issue_label"`). Lỗi một phần trả exit `2` (`status="partial_error"`).
- **Dashboard trực tiếp & Cross-provider audit (`aaf watch`, `[cross_audit]`)**:
  - `aaf watch [--repo <path>|--all] [--interval N] [--once]`: 100% chỉ đọc (mở DB WAL trên thư mục chỉ đọc bằng `mode=ro&immutable=1` khi không có `-wal`, không tạo `-shm`/`receipt.key`), 0 cổng mạng, không giữ khoá độc quyền, lọc ký tự ANSI/OSC trước khi in; hiển thị trực tiếp 4 bảng (task theo trạng thái + `unblock_cycles`/`blocked_reason`, khe nặng `free`/`busy`/`orphan`/`stale` + `drain` + áp lực RAM, pool account + `tokens_5h`, biên nhận ký gần nhất).
  - `[cross_audit]` (tầng 1 và tầng 2; tầng 3 chỉ siết được `require_different_provider = true`): cho phép tách auditor sang provider khác tác giả (vẫn cô lập trong worktree sạch của `base`, Seatbelt sandbox và env allow-list); `require_different_provider = true` fail-closed nếu không có provider khác.
  - `aaf doctor` trên macOS thêm cảnh báo advisory `keychain_sandbox_exposure` (ghi rõ kết quả đo `sandbox-exec` + `claude -p` trong `SECURITY.md` §10 #3).
  - Phát hiện Flutter bảo thủ (`pubspec.yaml` có `sdk: flutter` + thư mục `test/` mới điền `flutter test`); `commands.repro` giữ nguyên chế độ thủ công (`ReproCommand::argv`), không tự động thực thi.
  - **Chưa nghiệm thu thật**: build Android/Unity/Xcode thật chạy bên trong Seatbelt sandbox và luồng tạo PR / Projects v2 live tới GitHub remote vẫn mới có test trên fixture/đồ giả.
- **Gói 15 domain & UI/UX skills đa host (`aaf skills list|sync`, `aaf init --with-skills`) & kiểm tra `aaf doctor`**:
  - Nhúng **15 `SKILL.md`** lọc theo profile dự án: **6 lõi** dùng chung mọi profile + `auto` (`aaf-workflow`, `tdd-workflow`, `merge-conflict-resolver`, `qa-review`, `crash-triage`, `security-gate`), bộ UI/UX 4 skill đã tách vai trò không trùng lặp (`ui-ux-pro-max`, `taste-mobile-app`, `impeccable`, `qa-visual`), mobile + unity (`mobile-proof-qa`, `store-compliance`), Unity (`unity-game-craft` với mẫu Zero-GC C#), Web (`web-frontend-qa`), và Backend/Rust (`backend-service-gate`). Tổng theo profile: `auto` = 6, `backend`/`rust` = 7, `web` = 10, `unity` = 10, mobile (`android`/`ios`/`flutter`/`react-native`/`expo`) = 12.
  - Hỗ trợ `--host <claude|agents|all>` (`.claude/skills`, `.agents/skills`, hoặc cả hai), ghi nguyên tử (temp file + rename), chặn symlink dọc toàn bộ đường dẫn cha, phân tách `unchanged` vs `skipped_modified`, lan truyền lỗi `.autofactory/config.toml` hỏng thay vì nuốt lỗi, và thêm 3 mục advisory trong `aaf doctor` (`skills_sync`, `proof_readiness`, `github_board`).
  - `AGENT_CONFIG_GLOBS` bảo vệ cả `**/.agents/**` và `**/AGENTS.md` bên cạnh `**/.claude/**`, `**/CLAUDE.md`, `**/.mcp.json`.
- **Tách task chủ động đa subagent & tự chỉnh độ song song (`aaf board split`, `aaf run --auto-split`, `--max-parallel auto`)**:
  - `aaf board split <ID> [--max 2..=4] [--dry-run] [--unsandboxed]`: chạy planner chỉ đọc trong sandbox để phân rã task gốc (`Ready`/`Backlog`, `depth == 0`) thành 2..=4 subtask có `owned_paths` rời nhau giữa các nhánh chạy song song và tự nối DAG (task gốc chờ toàn bộ subtask). `--dry-run` chỉ xem trước, không ghi DB; bị từ chối (exit 2) trong `AAF_RUN_ID` hoặc với subtask/task không ở `Ready`/`Backlog`.
  - `aaf run --auto-split`: trước khi chạy vòng factory, tự chạy planner tách các task gốc `Ready` chưa có subtask và chưa có ghi chú `split:`; báo `auto_split_count` trong JSON và dừng sạch (`status: "halted"`, exit 2) nếu gặp lỗi hạ tầng.
  - `aaf run --max-parallel auto`: tự tính trần song song hiệu dụng (1..=4) từ số account phát hiện được, `max_concurrent_per_account`, số lõi CPU và áp lực RAM hiện tại của kernel (`Normal`/`Warn`/`Critical`), xuất `max_parallel` trong JSON.

## Bổ sung `v3.2` GOLD

- **DAG, khoá phạm vi và pipeline 4 chặng**: `aaf board dep`/`graph` (đường găng `critical path`, `blocked_upstream`), khoá phạm vi `--scope` so không phân biệt hoa thường kiểu APFS, `plan_stage` chỉ-đọc + `aaf board approve-plan`, vòng `audit_rework:` có giới hạn, phanh hàng đợi `[run].max_open_review`, squash-merge đã xác minh, `aaf gc`, và migration DB `v3 -> v4` (đọc chỉ-đọc tương thích cả v3 và v4).
- **7 Chuẩn Vàng và báo cáo bàn giao**: `[cross_audit]` đơn điệu 3 tầng (`audit_scope`: `head_lock < large < always`, `score_gate`: `bool` `false -> true`, `min_average_score` mặc định `95`, `min_criterion_score` mặc định `90`), `G1..G7` + `MechanicalScore` với huy hiệu `same_account — chưa có auditor độc lập` không gây deadlock cho solo-dev, verdict hậu gate ký HMAC `"audit\0"`, `aaf handover` (`md`/HTML có CSP kèm PNG đã xác minh `<= 4 MiB`/`json`), và PR body đã lọc bí mật (`< 60 KB`).
- **Stream log, snapshot lùi lượt và Cockpit TTY**: log trực tiếp đã lọc secret giới hạn `2 MiB` (`streams/<run_id>.jsonl`), snapshot trước mỗi lượt (`refs/aaf/snapshots/*` + bảng `turn_snapshots`) được bảo vệ khỏi agent, `aaf board undo`/`steer`, và `aaf cockpit` TTY đồng bộ (0 cổng mạng, 0 crate mới, lọc ANSI/OSC).

## Bổ sung `v3.4`

- **Remote Telegram companion (`aaf telegram`)**: long-polling outbound tới Telegram Bot API (0 cổng nghe, chỉ đọc `[telegram]` tầng 1, fail-closed khi `allowed_chat_ids` rỗng, token chỉ đọc từ biến môi trường) cho `/status`, `/board`, `/show`, `/handover` chỉ-đọc và xác nhận 2 bước `/confirm <code>` cho thao tác duyệt/thay đổi.
- **Adapter hook đa host**: `aaf-hook --host <auto|claude|codex|gemini>` và `aaf init --hook-host <claude|codex|gemini|all>` mở rộng toàn bộ bộ guard sang Codex CLI (`.codex/hooks.json`, gồm kiểm đường dẫn `apply_patch`) và Gemini CLI (`.gemini/settings.json`, sự kiện `BeforeTool`).
- **Che bí mật theo luồng, đóng gói và Linux `bwrap`**: lọc `[secret NAME]` qua ranh giới chunk trên stream output; phát hành đa nền tảng qua `.github/workflows/release.yml` + `scripts/install.sh` (kiểm SHA-256) + công thức Homebrew (`packaging/homebrew/aaf.rb`) + Docker runner cứng hoá (`packaging/docker/`); bộ dựng policy bubblewrap (`bwrap`) và nhận diện container cho Linux (chưa được `aaf run` dùng; Linux cần `--unsandboxed`); và kiểm chứng Seatbelt thực tế trên macOS (`docs/VERIFICATION_LIVE.md`, `scripts/live_verify.sh`).
