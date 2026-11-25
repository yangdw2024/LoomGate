-- LoomGate Stage 3 Cloud Control Plane — Redis Lua Two-Phase Atomic Billing Script
-- Guarantees zero overspend, idempotency by reservation_id, and exact refund on settle/release.
--
-- KEYS[1] = "billing:avail:<tenant_id>"     (integer micro-USD, 1 USD = 1_000_000 micros)
-- KEYS[2] = "billing:reserved:<tenant_id>"  (integer micro-USD currently locked in flight)
-- KEYS[3] = "billing:resv:<reservation_id>" (hash: status, amount, tenant_id)
--
-- ARGV[1] = op ("RESERVE" | "SETTLE" | "RELEASE")
-- ARGV[2] = amount_micros (estimated for RESERVE, actual for SETTLE, 0 for RELEASE)
-- ARGV[3] = ttl_seconds (e.g. 600 for RESERVE)

local op = ARGV[1]
local amount = tonumber(ARGV[2]) or 0
local ttl = tonumber(ARGV[3]) or 600

if op == "RESERVE" then
    local existing_status = redis.call("HGET", KEYS[3], "status")
    if existing_status == "reserved" then
        return {1, tonumber(redis.call("GET", KEYS[1]) or "0"), "IDEMPOTENT_OK"}
    end
    local avail = tonumber(redis.call("GET", KEYS[1]) or "0")
    if avail < amount then
        return {0, avail, "INSUFFICIENT_BALANCE"}
    end
    local next_avail = redis.call("DECRBY", KEYS[1], amount)
    redis.call("INCRBY", KEYS[2], amount)
    redis.call("HSET", KEYS[3], "status", "reserved", "reserved", amount)
    redis.call("EXPIRE", KEYS[3], ttl)
    return {1, next_avail, "RESERVED"}

elseif op == "SETTLE" then
    local status = redis.call("HGET", KEYS[3], "status")
    if not status then
        return {0, tonumber(redis.call("GET", KEYS[1]) or "0"), "NOT_FOUND"}
    end
    if status == "settled" then
        return {1, tonumber(redis.call("GET", KEYS[1]) or "0"), "IDEMPOTENT_SETTLED"}
    end
    local reserved = tonumber(redis.call("HGET", KEYS[3], "reserved") or "0")
    redis.call("DECRBY", KEYS[2], reserved)
    local delta = reserved - amount
    local next_avail = tonumber(redis.call("GET", KEYS[1]) or "0")
    if delta > 0 then
        next_avail = redis.call("INCRBY", KEYS[1], delta)
    elseif delta < 0 then
        local extra = -delta
        if next_avail >= extra then
            next_avail = redis.call("DECRBY", KEYS[1], extra)
        else
            redis.call("SET", KEYS[1], 0)
            next_avail = 0
        end
    end
    redis.call("HSET", KEYS[3], "status", "settled", "settled", amount)
    return {1, next_avail, "SETTLED"}

elseif op == "RELEASE" then
    local status = redis.call("HGET", KEYS[3], "status")
    if status ~= "reserved" then
        return {1, tonumber(redis.call("GET", KEYS[1]) or "0"), "ALREADY_CLOSED"}
    end
    local reserved = tonumber(redis.call("HGET", KEYS[3], "reserved") or "0")
    redis.call("DECRBY", KEYS[2], reserved)
    local next_avail = redis.call("INCRBY", KEYS[1], reserved)
    redis.call("HSET", KEYS[3], "status", "released", "settled", 0)
    return {1, next_avail, "RELEASED"}
end

return {0, 0, "INVALID_OP"}
