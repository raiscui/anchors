# 0.6.0

- Moved a lot of internal machinery into `expert`. As a normal anchors user, you shouldn't need to use anything except stuff exported from `singlethread`!
- Added an optional feature `anchors_pending_queue` to enable the pending request queue (`PendingDefer` + drain). Default is off.
