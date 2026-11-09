-- Increment the counter and arm its expiry in one atomic step, so a counter
-- is always born carrying its deadline instead of waiting for a second call.
--
-- A stored negative or non-integer value is corruption, not a count: the script
-- refuses it with a CORRUPT reply and never writes, so the record keeps
-- rejecting every hit until its expiry removes it.
local current = redis.call('GET', KEYS[1])
if current ~= false then
  local value = tonumber(current)
  if value == nil or value < 0 then
    return redis.error_reply('CORRUPT counter under the key is not a non-negative integer')
  end
end

local next = redis.call('INCRBY', KEYS[1], ARGV[1])
redis.call('PEXPIRE', KEYS[1], ARGV[2])
return next
