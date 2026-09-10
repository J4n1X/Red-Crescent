-- Shared helpers, loaded from templates via require("utils").
-- .lua files under the serve directory are require-able but never served.
local M = {}

--- Hours of 8-hour work days left in the month, counting a partial day today.
function M.remaining_work_hours(today, last_day)
    local work_days = 0
    for day = today.day, last_day do
        local timestamp = os.time({ year = today.year, month = today.month, day = day })
        local weekday = os.date("*t", timestamp).wday
        if weekday ~= 1 and weekday ~= 7 then
            work_days = work_days + 1
        end
    end

    local remaining_hours_today = 0
    if today.wday ~= 1 and today.wday ~= 7 then
        local current_hour = tonumber(os.date("%H"))
        remaining_hours_today = math.max(0, math.min(17 - current_hour, 8))
        work_days = work_days - 1 -- today is counted separately
    end
    return work_days * 8 + remaining_hours_today
end

return M
