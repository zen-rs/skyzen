-- Increment the counter and arm its expiry in one atomic step, so a counter
-- is always born carrying its deadline instead of waiting for a second call.
local next = redis.call('INCRBY', KEYS[1], ARGV[1])
redis.call('PEXPIRE', KEYS[1], ARGV[2])
return next
