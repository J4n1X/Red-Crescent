-- Writes three ticks with pauses, then exits (frees its name).
local db = sqlite.open("threadtest/worker.db")
db:execute("CREATE TABLE IF NOT EXISTS ticks (id INTEGER PRIMARY KEY)")
for i = 1, 3 do
    db:execute("INSERT INTO ticks DEFAULT VALUES")
    sleep(0.2)
end
db:close()
