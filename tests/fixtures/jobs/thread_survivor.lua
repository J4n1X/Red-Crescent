-- Total awake time exceeds the 400ms test timeout, but each awake stretch
-- stays under it because sleep() resets the deadline. The final mark proves
-- the thread survived.
local db = sqlite.open("threadtest/survivor.db")
db:execute("CREATE TABLE IF NOT EXISTS marks (id INTEGER PRIMARY KEY)")
local function busy(ms)
    local t = os.clock()
    while (os.clock() - t) * 1000 < ms do end
end
for i = 1, 3 do
    busy(250)
    sleep(0.05)
end
db:execute("INSERT INTO marks DEFAULT VALUES")
db:close()
