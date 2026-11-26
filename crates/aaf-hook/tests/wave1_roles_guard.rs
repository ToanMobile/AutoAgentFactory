//! Wave 1 hook guard tests: `.autofactory/roles/**` protection under APFS folding.

use std::path::Path;

use aaf_hook::paths::protected_name;

#[test]
fn autofactory_roles_files_are_protected_case_insensitively() {
    assert_eq!(
        protected_name(Path::new("/repo/.autofactory/roles/implementer.toml")),
        Some(".autofactory/roles/*")
    );
    assert_eq!(
        protected_name(Path::new("/repo/.AUTOFACTORY/ROLES/nested/custom.toml")),
        Some(".autofactory/roles/*")
    );
    assert_eq!(
        protected_name(Path::new("/repo/src/roles/implementer.toml")),
        None
    );
}
