-- One unbroken 600ms stretch of work, no sleep() anywhere: far past the 400ms
-- request timeout. With no thread deadline configured (the default) it has to
-- finish; the per-request budget would have killed it.
local db = sqlite.open("threadtest/longrun.db")
db:execute("CREATE TABLE IF NOT EXISTS marks (id INTEGER PRIMARY KEY)")
local t = os.clock()
while (os.clock() - t) * 1000 < 600 do end
db:execute("INSERT INTO marks DEFAULT VALUES")
db:close()
