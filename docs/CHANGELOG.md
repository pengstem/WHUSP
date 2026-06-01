## 2026-06-01

### Achievements
* `8d4ef5ea2a7cf4a9dcec197bd3002da077d48f75`: chore(bench): let us run the bench. Added the lwext4_rust library to run the bench and support filesystem operations.

### Shortcomings / Unresolved Issues
* Sparse file extension only records the new file size and zeroes the old tail block.
  * **Suggested Solution:** Implement the full Linux sparse-file behavior for every indirect-block layout and read-back edge case.
* RESTART2, KEXEC, and SW_SUSPEND require reboot strings, kernel-image handoff, or suspend support.
  * **Suggested Solution:** Implement reboot strings, kernel-image handoff, or suspend support in the kernel.
* UTS namespaces and sethostname/setdomainname are not implemented.
  * **Suggested Solution:** Implement full UTS namespaces and sethostname/setdomainname.
