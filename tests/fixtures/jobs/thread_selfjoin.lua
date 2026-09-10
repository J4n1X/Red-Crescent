-- A thread joining itself would block until the cap; it must be refused.
local ok, err = pcall(thread.join, thread.id(), 1)
return { ok = ok, err = tostring(err) }
