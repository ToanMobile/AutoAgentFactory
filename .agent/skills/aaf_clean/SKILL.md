---
name: aaf_clean
description: >-
  Dọn dẹp toàn diện cho AutoAgentFactory (AAF) và các repo đang phát triển: chạy
  `aaf gc --all --merged-branches`, xoá các worktree sạch/đã merge (`.aaf-worktrees`,
  `.claude/worktrees`, `aaf-wt/target-*` & `aaf-wt/aaf-*` nhàn rỗi), xoá tất cả
  local git branch thừa đã được merge vào `main`/`master`/`develop`, và tuỳ chọn
  dọn cache build an toàn — nhưng bảo toàn 100% worktree/branch đang làm dở
  (`dirty`, `locked`, chưa merge) và tiến trình `aaf`/`cargo`/`gradle` đang chạy.
  Kích hoạt khi gõ `/aaf_clean`, `/aff_clean`, `/aaf-clean`, "dọn dẹp aaf",
  "dọn worktree aaf", hoặc "dọn branch đã merge".
---

# AAF Clean (`/aaf_clean` / `/aff_clean`)

Skill dọn dẹp an toàn toàn bộ worktree dư thừa, local git branch đã merge vào nhánh chính, và các thư mục `target-*` / `aaf-*` nhàn rỗi sinh ra trong quá trình chạy AutoAgentFactory (`aaf`) và subagents.

> [!CAUTION]
> - Tuyệt đối **KHÔNG** đụng vào `/Volumes/Data/Toan/agent-workbench` (repo đã blacklisted, kiểm tra qua đường dẫn chuẩn hoá `pwd -P`).
> - **KHÔNG** xoá worktree đang bị `locked`, đang có thay đổi chưa commit (`dirty`), đang có commit chưa merge vào nhánh chính (kể cả merge commit), mới sửa trong `--min-age-mins`, hoặc đang có tiến trình chạy bên trong.
> - **KHÔNG** xoá nhánh chính (`main`, `master`, `develop`, `release/*`), nhánh đang checked out ở bất kỳ worktree nào, hoặc nhánh còn commit chưa merge.
> - Chế độ `--dry-run` chỉ đọc và thống kê, **tuyệt đối không** sửa/prune metadata Git hay xoá file trên đĩa.

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

### Dọn kèm luôn cache Gradle cũ / Xcode DerivedData / npm / uv
```bash
bash ~/.gemini/config/skills/aaf_clean/scripts/aaf_clean.sh --include-caches
```

### Tuỳ chọn khác
| Cờ | Tác dụng |
|---|---|
| `--dry-run` | Chỉ liệt kê worktree, branch, `target-*` sẽ xoá, **không** sửa hay xoá gì trên đĩa. |
| `--repo <PATH>` | Chỉ dọn 1 repo cụ thể (tự động giới hạn `aaf gc --repo`, bỏ qua Step 2 `aaf-wt/*` và Step 5 system caches). |
| `--include-caches` | Dọn thêm Gradle `build-cache-1`/`daemon`, Xcode `DerivedData`, `npm`/`uv` cache (chỉ xoá khi tool tương ứng đang rảnh). |
| `--min-age-mins <N>` | Chỉ xoá `worktrees` và `aaf-wt/target-*`/`aaf-*` không có process nào giữ và không đụng tới trong N phút (mặc định: `120` phút). |

---

## 2. Quy trình script thực hiện

1. **Chạy `aaf gc`**: Gọi `aaf gc --all --merged-branches` (hoặc `--repo <PATH> --merged-branches`, kèm `--dry-run` nếu có) để dọn `.aaf-worktrees`, batch mồ côi, snapshot refs và log cũ chuẩn của AutoAgentFactory.
2. **Dọn `aaf-wt/target-*` & `aaf-wt/aaf-*` nhàn rỗi** (tại `/Volumes/Data/Toan/aaf-wt`, bỏ qua khi có `--repo`):
   - Kiểm tra `pgrep` và `lsof` (giới hạn ranh giới đường dẫn chính xác) xem có tiến trình nào đang giữ file bên trong không.
   - Kiểm tra đệ quy bên trong xem có file nào mới sửa trong `--min-age-mins` (mặc định 120 phút) không.
   - Với `aaf-*` phải sạch (`dirty = 0`), không `locked`, không thuộc repo blacklisted, và đã merge vào `origin/main`/`main`/`origin/master`/`master`/`origin/develop`/`develop`; dùng `git worktree remove` trên repo cha nếu là linked worktree.
3. **Dọn `.claude/worktrees` sạch & đã merge** (quét các repo dưới `/Volumes/Data/{Toan,GalaxyHolding,Projects}`, trừ `agent-workbench`):
   - Fail-closed: chỉ xoá khi worktree **không bị `locked`**, không có tiến trình đang chạy bên trong, không mới sửa trong `--min-age-mins`, `git status --porcelain` trống (`dirty = 0`), **VÀ** `HEAD` đã merge vào `base_branch` (`merge-base --is-ancestor`, hoặc không có merge commit và 0 commit chưa merge qua `git cherry`).
4. **Dọn local git branch thừa đã merge**:
   - Xác định `base_branch` (`main` / `master` / `develop` / `origin/HEAD`).
   - Dùng `git for-each-ref --merged` và `git worktree list --porcelain` để xoá các local branch đã merge hoàn toàn, **bảo toàn** `main`, `master`, `develop`, `release/*` và mọi nhánh đang được check out ở bất kỳ worktree nào.
5. **Dọn cache hệ thống tuỳ chọn (`--include-caches`)**: Chỉ chạy khi không truyền `--repo` và kiểm tra tiến trình `gradle`, `xcodebuild`/`Xcode`, `npm`/`uv` đang rảnh.
6. **Báo cáo dung lượng**: In `aaf_wt_matched`, `worktrees_matched`, `merged_branches_matched` và dung lượng trống trước/sau (`df -h`).

---

## 3. Quy trình khi agent thực thi skill này

1. Luôn chạy `--dry-run` trước nếu người dùng chưa xác nhận phạm vi xoá, hoặc tóm tắt rõ danh sách sẽ xoá trước khi chạy thật.
2. Không bao giờ thêm `--force` vào `git worktree remove` hoặc `git branch -D` trừ khi người dùng yêu cầu tường minh cho một worktree/nhánh cụ thể.
