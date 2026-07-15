# Bali redeploy runbook (retired)

The deployment-specific procedure formerly stored here no longer describes the
current service and has been retired. It intentionally contains no executable
recovery steps.

The filename remains because the already-applied
`007_monitor_state_persistence.sql` migration contains a historical comment
that names it; applied migrations are checksum-protected and must not be edited.

Use the current, deployment-neutral documents instead:

- [Operations runbook](operations/runbook.md)
- [Diagnostics](operations/diagnostics.md)
- [Upgrade guide](UPGRADE.md)
- [Historical IAIC/AccountDataNotFound postmortem](POSTMORTEM_2026-05-11_IAIC_TO_ADNF.md)
