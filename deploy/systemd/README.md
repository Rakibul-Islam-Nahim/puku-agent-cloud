# systemd units

| unit | what |
|---|---|
| `puku-controld.service` | control plane, :7770 |
| `puku-workerd.service` | worker daemon (needs `/dev/kvm`) |

## Memory

`puku-controld` reads two variables for the memory service:

```
PUKU_MEMORY_URL          set in the unit (default http://127.0.0.1:7970)
PUKU_MEMORY_SERVICE_KEY  set in /etc/puku/memory.env
```

`/etc/puku/memory.env` is loaded with a leading `-`, so a deployment without
memory simply omits the file:

```
# /etc/puku/memory.env   (chmod 600, owned by puku)
PUKU_MEMORY_SERVICE_KEY=<the same key the memory service is given>
```

**Without the key, memory stays off.** controld logs a warning naming the
missing variable rather than failing every preamble with a 401 that looks like
the memory service being broken.

The memory service itself ships its own unit in `puku-memory-service/deploy/systemd/`.
It must be given the *same* `PUKU_MEMORY_SERVICE_KEY`.

## Rotating the service key

The memory service accepts two keys at once, so a rotation has an overlap
window instead of an outage:

1. set `PUKU_MEMORY_SERVICE_KEY_PREV` on the memory service to the current key
2. set `PUKU_MEMORY_SERVICE_KEY` on the memory service to the new key, restart
3. roll controld to the new key
4. clear `PUKU_MEMORY_SERVICE_KEY_PREV`, restart
