---
name: aaf_clean
description: >-
  Dọn dẹp toàn diện cho AutoAgentFactory (AAF) và các repo đang phát triển: chạy
  `aaf gc --all --merged-branches`, xoá các worktree sạch/đã merge (`.aaf-worktrees`,
  `.claude/worktrees`, `aaf-wt/*`), xoá tất cả local git branch thừa đã được merge
  vào `main`/`master`/`develop`, và dọn các thư mục Rust `target-*` / `build/` nhàn
  rỗi không còn tiến trình nào sử dụng — nhưng bảo toàn 100% worktree/branch đang
  làm dở (`dirty` hoặc còn commit chưa merge) và tiến trình `aaf`/`cargo`/`gradle`
  đang chạy. Kích hoạt khi gõ `/aaf_clean`, `/aff_clean`, `/aaf-clean`, "dọn dẹp
  aaf", "dọn worktree aaf", hoặc "dọn branch đã merge".
---

# AAF Clean (`/aaf_clean` / `/aff_clean`)

Skill dọn dẹp an toàn toàn bộ worktree dư thừa, local git branch đã merge vào nhánh chính, và các thư mục `target-*` / build tạm sinh ra trong quá trình chạy AutoAgentFactory (`aaf`) và subagents.

> [!CAUTION]
> - Tuyệt đối **KHÔNG** đụng vào `/Volumes/Data/Toan/agent-workbench` (repo đã blacklisted).
> - **KHÔNG** xoá worktree đang có thay đổi chưa commit (`dirty`), đang có commit chưa merge vào nhánh chính (`unmerged > 0`), hoặc đang có tiến trình (`aaf`, `cargo`, `rustc`, `gradle`) chạy bên trong.
> - **KHÔNG** xoá nhánh chính (`main`, `master`, `develop`, `release/*`), nhánh hiện tại đang checkout, hoặc nhánh còn commit chưa merge.

---

## 1. Cách dùng nhanh

### Xem trước những gì sẽ bị xoá (khuyến nghị kiểm tra trước)
```bash
bash ~/.gemini/config/skills/aaf_clean/scripts/aaf_clean.sh --dry-run
```

### Chạy dọn dẹp thực tế (Worktrees + Merged Branches + `aaf-wt/target-*` nhàn rỗi)
```bash
bash ~/.gemini/config/skills/aaf_clean/scripts/aaf_clean.sh
```

### Dọn kèm luôn cache Gradle cũ / Xcode DerivedData
```bash
bash ~/.gemini/config/skills/aaf_clean/scripts/aaf_clean.sh --include-caches
```

### Tuỳ chọn khác
| Cờ | Tác dụng |
|---|---|
| `--dry-run` | Chỉ liệt kê worktree, branch, `target-*` sẽ xoá, **không** sửa hay xoá gì trên đĩa. |
| `--repo <PATH>` | Chỉ dọn 1 repo cụ thể thay vì quét toàn bộ repo trong hệ thống. |
| `--include-caches` | Dọn thêm cache Gradle (`build-cache-1`, `daemon`), Xcode `DerivedData`, `npm`/`uv` cache. |
| `--min-age-mins <N>` | Chỉ xoá `aaf-wt/target-*` không có process nào giữ và không đụng tới trong N phút (mặc định: `120` phút). |

---

## 2. Quy trình script thực hiện

1. **Chạy `aaf gc`**: Gọi `aaf gc --all --merged-branches` (hoặc `--dry-run`) để dọn worktree, batch mồ côi, snapshot refs và log cũ chuẩn của AutoAgentFactory.
2. **Dọn `aaf-wt/target-*` & `aaf-wt/aaf-*` nhàn rỗi** (mặc định tại `/Volumes/Data/Toan/aaf-wt`):
   - Kiểm tra `pgrep` và `lsof -c cargo -c rustc -c aaf -c java -c gradle` xem có tiến trình nào đang giữ file bên trong không.
   - Kiểm tra đệ quy bên trong thư mục xem có file nào mới sửa trong `--min-age-mins` (mặc định 120 phút) không; với `aaf-*` phải sạch (`dirty = 0`) và đã merge hết vào `origin/main`/`main`.
3. **Dọn `.claude/worktrees` sạch & đã merge** (trừ `agent-workbench`):
   - Fail-closed: chỉ xoá khi worktree **không bị `locked`**, không có tiến trình đang chạy bên trong, `git status --porcelain` trống (`dirty = 0`) **VÀ** `HEAD` đã merge vào `base_branch` (`merge-base --is-ancestor` hoặc 0 commit chưa merge).
   - Với các worktree còn đang làm (`dirty > 0`, `unmerged > 0`, hoặc `locked`), giữ nguyên 100% mã nguồn & nhánh Git.
4. **Dọn local git branch thừa đã merge**:
   - Với mỗi repo hợp lệ, xác định base branch (`main` / `master` / `develop`).
   - Xoá các local branch đã merge hoàn toàn (`git branch --merged <base>`), trừ `main`, `master`, `develop`, nhánh đang gắn vào worktree khác (`+`), và nhánh hiện tại (`*`).
   - Chạy `git worktree prune` để làm sạch metadata.
5. **Báo cáo dung lượng**: In tóm tắt số lượng worktree, branch, `target-*` đã xoá và dung lượng SSD trống trước/sau (`df -h`).
