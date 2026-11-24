# Mẫu Prompt cho Chế độ 1 (Standard Audit)

> Đọc khi tới Bước 3 (Mẫu A), Bước 4b (Mẫu V), Bước 5 (Mẫu B), hoặc Bước 6 (Mẫu C).

---

## Mẫu A — QUÉT (CHỈ ĐỌC, chạy trong worktree sạch do PM tạo sẵn)

````markdown
# QUÉT <lane> — aaf — <YYYY-MM-DD> — HEAD <sha>

Bạn là thợ audit, CHỈ ĐỌC.
Worktree sạch tại HEAD <sha> đã được PM tạo sẵn và đặt làm thư mục làm việc hiện tại.
KHÔNG chạy `git worktree`, KHÔNG ghi/sửa bất kỳ file nào, KHÔNG đọc file cấu hình/credential trong `$HOME`. In TOÀN VĂN báo cáo theo mẫu bên dưới ra stdout.
Đọc trước: `docs/PLAN.md` §B–C, `SECURITY.md`.

## Câu hỏi chính
<từ bảng lane>

## Phạm vi
<danh sách file/thư mục của lane>. `E/` = `crates/aaf-engine/src/`.

## Số đo hiện tại
<các dòng liên quan từ metrics.md>

## Đã biết — không báo lại
<Khoá — trạng thái — lý do một dòng (gồm cả `loại` và `chờ User` / `RFC`)>
Chỉ báo lại khi có bằng chứng MỚI chưa nêu trong lý do.

## Luật báo cáo (2 Kênh)
- **Kênh A (`[CH-A]`, tự sửa được):** Báo tối đa **5 phát hiện**, xếp theo mức. **0 phát hiện Kênh A là kết quả hợp lệ và được đánh giá tốt.** Mỗi phát hiện Kênh A bị loại đều tính vào tỉ lệ sai của model bạn. Phải có đầu vào cụ thể làm hành vi quan sát được sai (không báo mẫu grep trần).
- **Kênh B (`[CH-B]`, Strategic RFC — `Chờ User duyệt`):** Báo tối đa **3 cơ hội chiến lược** (tối ưu kiến trúc, rút gọn workflow Auto-100%, tăng tốc gate phút→giây, dọn worktree/tài nguyên 24/7, thêm guard Zero-Touch `main`, nới false-positive có thiết kế giữ nguyên `SECURITY.md`, hoặc lỗi chạm Vùng cấm). CẤM tự giấu phát hiện chỉ vì chạm Vùng cấm.
- Đọc kỹ tối đa khoảng 12 file hoặc 4000 dòng. Phần vượt ghi vào cột "Chưa đọc". Không ghi "đã quét đủ" khi còn phần chưa đọc.
- Mức độ: Critical = fail-open hoặc lách guard mà agent trong sandbox với tới được; High = chặn oan trên đường phổ biến, mất dữ liệu, hoặc task kẹt; Medium = đường hiếm, hoặc chậm ≥ 10% có số đo; Low = phần còn lại. Không có "Đường tới" thì tối đa Medium.

## Lệnh tìm (chỉ là đầu mối — dùng tool `Grep` hoặc chạy từng lệnh `git grep` ĐỘC LẬP, không gán biến/nối `;` để khớp allowlist `Bash(git grep:*)`)
`git grep -E` không hiểu `\w` và `\b`: dùng `[A-Za-z0-9_]` và `-w`, hoặc `git grep -P`.
Lệnh ra 0 kết quả thì chạy lại bằng một mẫu chắc chắn có kết quả trước khi kết luận "không có".
Thay `<paths>` bằng danh sách đường dẫn của lane:
```bash
git grep -nE 'let _ = |\.ok\(\);|unwrap_or_default\(\)|unwrap_or\((true|false)\)' -- <paths> ':!*/tests/*' ':!*tests.rs'
git grep -nE 'panic!\(|unreachable!\(|#\[allow\(clippy::(unwrap|expect)_used' -- <paths> ':!*/tests/*' ':!*tests.rs'
git grep -nE ' as (u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|isize|f32|f64)([^A-Za-z0-9_]|$)' -- <paths> ':!*/tests/*' ':!*tests.rs'
git grep -nE 'Command::new|\.spawn\(|"-c"|\.wait\(|\.kill\(|killpg|process_group' -- <paths> ':!*/tests/*' ':!*tests.rs'
git grep -nE 'canonicalize|symlink_metadata|\.exists\(\)|set_permissions|OpenOptions|rename\(|remove_dir_all' -- <paths> ':!*/tests/*' ':!*tests.rs'
git grep -nE 'try_lock|\.lock\(\)|timeout|thread::sleep|recv\(\)|\.join\(\)' -- <paths> ':!*/tests/*' ':!*tests.rs'
git grep -niE 'env::var|env_remove|env_clear|\.envs?\(|token|secret' -- <paths> ':!*/tests/*' ':!*tests.rs'
git grep -nE '\.clone\(\)|collect::<Vec|read_to_string|read_to_end' -- <paths> ':!*/tests/*' ':!*tests.rs'
```
Lưu ý:
- Khối `#[cfg(test)] mod tests` viết trong file `src/` không bị loại; tự bỏ qua khi đọc.
- Nếu lane có `templates/skills/**/*.md` (`obs`): kiểm tra mục `Headless / AAF_RUN_ID Mode`, chỉ thị ép mỗi task chạy trên worktree riêng (cấm sửa trực tiếp lên `main`), chỉ thị dừng hỏi user, lệnh host-only (`aaf gate|approve|proof record|merge|board|skills sync|gc|handover|run|init|plan|sop|cockpit|telegram|ui|update|upgrade`), và độ khớp tên cờ CLI thật.
- Nếu lane có `E/git/**`, `crates/aaf-hook/**`, `E/executors/**`, `crates/aaf-cli/**`: kiểm tra kỹ mọi đường có thể khiến phiên interactive hoặc worker fallback sửa thẳng lên `main`, ghi vào `<main>/.git/{HEAD,index,packed-refs,refs/heads/<protected>}`, hoặc làm bẩn working tree `main` (gồm cả `aaf sop run`).
- Nếu lane có `E/web_ui/index.html`, `packaging/**`, `scripts/**`, `.github/workflows/**`, `Cargo.toml` (`cli`): kiểm tra XSS/CSP, CSRF/auth header, đường dẫn copy/build thiếu `templates/` trong Docker/installer, và quyền CI (`permissions`/`persist-credentials`).

## Mẫu báo cáo (in thẳng ra stdout)
# <lane> — <YYYY-MM-DD> — <model> — HEAD <sha>
## Độ phủ
| File/thư mục | Số file | Đọc kỹ | Chỉ grep | Chưa đọc |
## Phát hiện Kênh A (`[CH-A]`)
### [<CAT>] <tiêu đề một dòng>
- Kênh: `[CH-A]`
- Khoá: `<file>::<fn>::<CAT>`
- Vị trí: `crates/.../file.rs:<dòng>` (hoặc `templates/...:<dòng>`)
- Trích nguyên văn (2–5 dòng):
  ```
  <code chép đúng từ sha>
  ```
- Mức: Critical | High | Medium | Low — Mục tiêu: gate đúng | worktree/main | gate nhanh | tự chủ | hiệu suất
- Đường tới (bắt buộc với Critical/High): <điểm vào> `file:dòng` → … → dòng lỗi
- Kịch bản: <đầu vào cụ thể> → <điều xảy ra> (so với <điều đúng ra phải xảy ra>)
- Phản chứng đã tìm: <đã kiểm caller hay test nào xem lỗi có bị chặn ở chỗ khác không, kèm file:dòng>
- Đề xuất: <thay đổi tối thiểu>. Nới guard: không. Chạm vùng cấm: không.
## Strategic RFC / Chờ User (`[CH-B]`)
### [RFC] <tiêu đề một dòng>
- Kênh: `[CH-B]`
- Khoá: `<file hoặc phạm vi>::<chủ đề>::RFC`
- Điểm nghẽn thực tế: <file:dòng + trích dẫn/số đo chứng minh>
- Tác động đo được: <giây tiết kiệm / số lệnh tay giảm / chất lượng đầu ra>
- Rủi ro bảo mật & thiết kế giữ nguyên `SECURITY.md`: <vùng cấm chạm tới, mối đe doạ và cơ chế giữ an toàn>
- Xếp hạng Impact × Effort: <High/Med/Low Impact × Low/Med/High Effort>
## Đã loại
<tưởng là lỗi nhưng không phải, kèm lý do>
````

---

## Mẫu V — XÁC MINH (CHỈ cho mã Kênh A; Kênh B RFC đi qua Red-Team, KHÔNG vào Mẫu V)

````markdown
# XÁC MINH <ID> — aaf — base <sha>

Bạn là thợ xác minh. Bạn chỉ được viết file test, cấm sửa code production.
Worktree: `git worktree add --detach /Volumes/Data/Toan/.aaf-audit-tmp/aaf-verify-<ID> <sha>`.
Dùng `CARGO_TARGET_DIR=/Volumes/Data/Toan/.aaf-audit-tmp/aaf-target-<ID>` (mồi bằng `cp -cR /Volumes/Data/Toan/AutoAgentFactory/target <thư mục đó>` nếu có).
*(Sau khi bạn báo cáo, PM sẽ tự kiểm lại, copy `<file test>` đóng băng sang worktree `audit/<ID>` ở Bước 5 và xoá `aaf-verify-<ID>`.)*

## Cần tái hiện
- Vị trí: <file:dòng>
- Kịch bản: <đầu vào → hành vi sai được cho là xảy ra>

## Việc
1. Viết một test nhỏ nhất có assertion mô tả **hành vi đúng**. Đặt test cạnh các test hiện có của crate (đảm bảo mọi file `.rs <= 600` dòng; với Kênh A sửa `templates/skills/**/*.md`, đặt test hồi quy tại `crates/aaf-engine/src/skills_tests.rs` kiểm tra nội dung `all_skills()`), rồi chạy `rustfmt <file test>`.
2. Chạy `cargo test -p <crate> <tên test>` (nếu `<crate>` là `aaf-cli`, bọc `AAF_REQUIRE_REAL=1`, `aaf slot`, và `python3` đặt `SIGHUP = SIG_DFL`). Kết quả mong đợi: ĐỎ ở assertion. Lỗi biên dịch không tính.
3. Sau một lượt mà test vẫn xanh: ghi "không tái hiện", kèm điều bạn quan sát thấy. Không cố ép cho đỏ.

## Báo cáo
Ghi `/Volumes/Data/Toan/AutoAgentFactory/docs/audit/<YYYY-MM-DD>/<NN>-<ID>-verify.md`, gồm:
- đường dẫn file test;
- toàn văn test;
- lệnh đã chạy;
- dòng `panicked at`/`assertion`, hoặc lý do không tái hiện.
````

---

## Mẫu B — SỬA (CHỈ cho mã Kênh A; PM chạy 2 lượt `claude -p` Pha 1 → `--resume` Pha 2)

````markdown
# SỬA <ID> — aaf — base <sha>

## Môi trường (đã được PM tạo sẵn)
- Worktree nhánh `audit/<ID>`: `/Volumes/Data/Toan/.aaf-audit-tmp/aaf-<ID>` (đã có sẵn file oracle `<file test>` đã đóng băng từ Bước 4b với đúng `git hash-object` = `<hash>`).
- `CARGO_TARGET_DIR=/Volumes/Data/Toan/.aaf-audit-tmp/aaf-target-<ID>`. Mồi bằng `cp -cR` từ target chính (không dùng chung với thợ khác).
- Test cả workspace hoặc `-p aaf-cli` chạy qua `AAF_REQUIRE_REAL=1 aaf slot --label ... -- python3` (reset `SIGHUP = SIG_DFL`).
- Báo cáo ghi ở checkout chính (`/Volumes/Data/Toan/AutoAgentFactory/docs/audit/...`), không đưa vào commit.

## Oracle (CẤM sửa)
`<file test>` — `git hash-object` = `<hash>`. Test này đang ĐỎ trên base.

## Pha 1 — Phản biện (CHỈ ĐỌC + chạy oracle)
1. Xác nhận `git hash-object <file test> == <hash>`, chạy oracle trên base và dán dòng đỏ. Không đỏ thì DỪNG và báo "không tái hiện".
2. Đọc kế hoạch và code thật. Liệt kê chỗ kế hoạch sai hoặc thiếu (`file:dòng — vấn đề`).
3. Nêu ít nhất 1 rủi ro hồi quy cụ thể, hoặc ghi rõ đã kiểm những chỗ nào.
4. Dừng, chờ PM duyệt (PM sẽ gọi tiếp Pha 2 qua `claude -p --resume "$SESSION_ID" --permission-mode acceptEdits ... "Làm"`).

## Kế hoạch của PM
- Hiện trạng: <file:dòng + bằng chứng>
- Các bước: <…>
- Được sửa: <file>
- CẤM sửa: <oracle, vùng cấm liên quan, mọi file khác>
- Rủi ro hồi quy: <…>

## Pha 2 — Thực hiện
1. Sửa production ở mức tối thiểu cho oracle chuyển XANH (giữ mọi file `.rs <= 600` dòng; tôn trọng `v32_cockpit_cli.rs` 8 module). Mã SEC/GATE thêm test cho nhánh cho qua hợp lệ.
2. Chạy `cargo fmt --all --check`, `aaf slot --label clippy-pkg -- cargo clippy -p <crate> --all-targets -- -D warnings`, và test crate (nếu sửa `templates/skills/**/*.md`, chạy `cargo test -p aaf-engine` + `aaf slot --label build-bins -- cargo build --workspace --bins`; nếu `<crate> == aaf-cli`, bọc `AAF_REQUIRE_REAL=1` + `aaf slot` + `SIGHUP = SIG_DFL`).
3. Với FAST/PERF: build binary trong worktree, gọi đường dẫn tuyệt đối + data dir tạm. Đo 1 làm nóng + 5 lần trước/sau, ghi trung vị.
4. Diff ≤ 400 dòng. Một commit duy nhất gồm file oracle đóng băng + bản sửa prod, dạng `type(scope): subject` bằng tiếng Anh. Không push.

## Xong khi
<oracle ĐỎ trên base, XANH trên nhánh; điều kiện đo nếu có>

## Báo cáo
`docs/audit/<YYYY-MM-DD>/<NN>-<ID>-result.md`: commit sha, lệnh kèm exit code, số đo, việc chưa làm và lý do.
````

---

## Mẫu C — PHẢN KIỂM (CHỈ ĐỌC, chạy bằng model KHÁC thợ sửa)

````markdown
# PHẢN KIỂM <ID> — aaf

Bạn là thợ phản kiểm, CHỈ ĐỌC. Bạn không được thấy kế hoạch, chỉ có phát hiện gốc và diff.
- Phát hiện gốc: <Kịch bản + Vị trí>
- Diff: `git diff <base>...audit/<ID>`

Trả lời ba câu, mỗi câu kèm đầu vào cụ thể hoặc `file:dòng`:
1. Sau bản sửa, còn đầu vào nào lách được guard hay tái hiện được lỗi không?
2. Ở chỗ khác trong repo có lời gọi hay đoạn code cùng mẫu lỗi chưa được sửa không?
3. Có nhánh lỗi nào giờ ra CLEAR hoặc cho qua, mà trước bản sửa thì bị chặn không?

Không có gì thì ghi "Không có" cho câu đó. In thẳng ra stdout (hoặc ghi `docs/audit/<YYYY-MM-DD>/<NN>-<ID>-counter.md`).
````
