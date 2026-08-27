-- Replace KEYS[1] only if it still holds what the caller expects.
--
-- Redis has no single command for this: GET and SET are two round trips, and anything can land
-- between them. Running the pair inside a script is what makes the read and the write one atomic
-- step, because Redis executes a script to completion before serving another client.
--
-- ARGV[1] is '1' when the caller expects the key to hold a value and '0' when it expects the key to
-- be absent. ARGV[2] carries the expected bytes and is unused when ARGV[1] is '0'. ARGV[3] is the
-- new value. Lua strings are binary-safe, so a value with embedded NUL bytes still compares
-- exactly.
--
-- Returns 1 when the swap was applied and 0 when the precondition no longer held. A lost race is
-- the ordinary outcome of an optimistic update, so it is reported rather than raised as an error.
--
-- The write is a plain SET, so the winner owns the key outright and any expiry the previous value
-- carried is cleared -- the same "a swap re-arms nothing" rule the in-memory and DynamoDB backends
-- follow.
local current = redis.call('GET', KEYS[1])

if ARGV[1] == '1' then
  if current == false or current ~= ARGV[2] then
    return 0
  end
elseif current ~= false then
  return 0
end

redis.call('SET', KEYS[1], ARGV[3])
return 1
