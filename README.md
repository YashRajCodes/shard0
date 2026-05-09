# shard0

shard0 is a high-performance Rust utility designed to solve a specific scaling bottleneck in the Discord API: the Shard 0 DM concentration. In the Discord gateway architecture, all Direct Messages (DMs) are routed exclusively to Shard 0. For large-scale bots, this single shard often becomes a performance bottleneck while other shards remain underutilized.

This project introduces a subsharding layer that intercepts Shard 0 traffic and redistributes it across multiple proxy instances based on DM channel IDs. This allows developers to scale DM processing horizontally without changing their bot's core logic.

## Configuration

### shard0

| Variable | Description |
| --- | --- |
| DISCORD_TOKEN | discord bot token |
| TOTAL_SHARDS | total shards the discord bot has (default: 2) |
| REDIS_URL | redis connection url (default: redis://127.0.0.1/) |
| TOTAL_SUBSHARDS | total number of subshards created (default: 4) |

### shard0-proxy

| Variable | Description |
| --- | --- |
| SUBSHARD | the subshard it should connect to |
| PORT | port the proxy listens at (default: 4343) |
| REDIS_URL | redis connection url (default: redis://127.0.0.1/) |

## How to use

### discord.py

```py
# import statements

WS_URL = "ws://127.0.0.1:4343"

get_bot_gateway = discord.http.HTTPClient.get_bot_gateway

async def custom_get_bot_gateway(self, *args, **kwargs):
    data = await get_bot_gateway(self, *args, **kwargs)
    data['url'] = WS_URL
    return data

discord.http.HTTPClient.get_bot_gateway = custom_get_bot_gateway

# rest of your code, e.g.:

bot = commands.Bot(command_prefix="!")
```

### discord.js

```js
const { Client } = require('discord.js');

const WS_URL = "ws://127.0.0.1:4343"

const client = new Client({
    // stuff
});

client.rest.get = new Proxy(client.rest.get, {
    apply(target, thisArg, args) {
        if (typeof args[0] === 'string' && args[0].includes('gateway')) {
            return Promise.resolve({
                url: WS_URL,
                shards: 1,
                session_start_limit: { total: 1000, remaining: 999, reset_after: 14400000, max_concurrency: 1 },
            });
        }
        return Reflect.apply(target, thisArg, args);
    }
});
```
