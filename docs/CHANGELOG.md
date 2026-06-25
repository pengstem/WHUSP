# Changelog

## 2026-06-14

### Achievements
- Chore(iozone): speed up my friend (62569c1)

### Shortcomings or Unresolved Issues
- **Issue**: Full Linux signal delivery must support every signal mechanism
  **Suggested Solution**: Implement full Linux signal delivery mechanisms.
- **Issue**: Linux also detects altstack overflow and reports SIGSEGV
  **Suggested Solution**: Add altstack overflow detection and SIGSEGV reporting.
- **Issue**: Linux also validates and restores vector extension records.
  **Suggested Solution**: Implement validation and restoration for vector extension records.
- **Issue**: Full SA_RESTART is not modeled yet.
  **Suggested Solution**: Model full SA_RESTART for interrupted operations.
- **Issue**: Linux accounts and reclaims memory per cgroup.
  **Suggested Solution**: Implement memory accounting and reclamation per cgroup.
- **Issue**: Linux stat timestamps should reflect filesystem time updates
  **Suggested Solution**: Ensure stat timestamps accurately reflect filesystem time updates.
- **Issue**: lwext4 exposes raw errno values that are not all mapped
  **Suggested Solution**: Map all raw errno values exposed by lwext4 to system error codes.
- **Issue**: The vendored lwext4 wrapper can create special inode
  **Suggested Solution**: Update lwext4 wrapper to fully support special inodes.
- **Issue**: Linux also keeps opened directories alive across unlink.
  **Suggested Solution**: Implement logic to keep opened directories alive across unlink.
- **Issue**: Linux keeps unlinked-but-open files alive.
  **Suggested Solution**: Implement logic to keep unlinked-but-open files alive.
