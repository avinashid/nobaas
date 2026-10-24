# @peerwire/react

React hooks for [Peerwire](../../README.md) rooms: join a peer-to-peer room, list its members, chat, and share
state across the room, with no server of your own. Written in TypeScript; the types ship with the package.

```sh
npm install @peerwire/react @peerwire/js
```

`react` (18 or later) and `@peerwire/js` are peer dependencies. The hooks are a thin layer over
[`@peerwire/js`](../js/README.md): see its README for what a room is, signalling, privacy and NAT traversal.

## A shared counter in a dozen lines

```tsx
import { RoomProvider, usePeers, useSharedState } from '@peerwire/react';

const named = (p: unknown): p is { name: string } => typeof (p as { name?: unknown })?.name === 'string';

export function App() {
  return (
    <RoomProvider appId="my-app" roomId="lobby" presence={{ name: 'Ana' }}>
      <Counter />
    </RoomProvider>
  );
}

function Counter() {
  const peers = usePeers({ accept: named }); // members whose presence has a text name
  const [count, setCount] = useSharedState('count', 0); // numbers only, as `initial` is one
  return <button onClick={() => setCount((c) => c + 1)}>{count} (with {peers.map((p) => p.presence.name).join(', ')})</button>;
}
```

Open it in two tabs: the counter is the same in both. `signaling: 'nostr'` (a `RoomProvider` prop, like every
`joinRoom` option) reaches other devices through public Nostr relays. Use `randomRoomId()` for a room only the
people you give its id can join.

## Hooks

| Hook | What it gives |
|---|---|
| `<RoomProvider {...joinRoomOptions}>` | Joins the room while mounted; the hooks below use it. Changing `presence` sends it; changing another option joins again. |
| `useRoom()` | `{ room, status, error }`: `room` is null until joined; `status` is `'joining'`, `'joined'` or `'error'`. |
| `usePeers({ accept })` | The other members, oldest first: `{ id, presence }[]`. |
| `usePresence()` | `[ours, setOurs]`, like `useState`; every member sees the change. |
| `useMessages({ limit, accept })` | `{ messages, send, clear }`. `messages` holds the newest `limit` (default 200), each `{ id, peer, data, lane, kind, at, mine }`; `send(data, { to, lane })` is `room.send`, and adds the message with `mine: true`. |
| `useRoomEvent(event, handler)` | Calls the latest `handler` for `'peer-join'`, `'peer-leave'`, `'presence'` or `'message'`. |
| `useSharedState(key, initial, { accept })` | `[value, setValue]` shared by the whole room (below). |
| `usePeerState(key, { accept })` | `[byPeer, setOurs]`: each other member's latest value, such as a cursor (below). |
| `usePeerwireRoom(options \| null)` | What `RoomProvider` uses: joins without a provider, returns `{ room, status, error }`; null stays out. |

Every hook but `useRoom` takes an options object last. Its `room` (a `Room`, or null for none) works on that room
instead of the nearest provider's. `randomRoomId` and the `@peerwire/js` types (`Room`, `PeerId`,
`JoinRoomOptions`, ...) are re-exported.

### Checking what members send

Another member can send any JSON in place of what your types say: an object where you expect a name, a string
where you expect a number. React throws when it renders an object as text, and with no error boundary the whole
app unmounts, which also leaves the room. So check every value from other members before you render it: give the
hooks an `accept` check. A type guard there also gives the hook its type, with no generic to write:

```tsx
const isPoint = (v: unknown): v is { x: number; y: number } => Number.isFinite((v as any)?.x) && Number.isFinite((v as any)?.y);

const { messages } = useMessages({ accept: (data) => typeof data === 'string' }); // Message<string>[]
const [cursors] = usePeerState('cursor', { accept: isPoint });                    // { [peer]: { x, y } }
```

What fails the check is kept out: a message isn't added, a member's value is left out of `usePeerState`, and a
member whose presence fails is left out of `usePeers`. A shared value that fails reads as `initial`, on every
member alike. `useSharedState` checks even without `accept`: a value must be of the kind `initial` is (the same
`typeof`, an array for an array, a non-array object for an object; anything when `initial` is null). The check
should be a plain function of the value; when it changes, the new one applies from the next change in the room.
A check that throws counts as failed.

### Shared state

`useSharedState('count', 0)` works like `useState`, for the whole room. A write goes to every member, and one
who joins later catches up from the others. When two members write at once, every member settles on the same
one: each write carries a logical clock and the highest wins, then the higher member id. Values travel as JSON.
Until the room is joined the value is `initial`, and setting it does nothing.

### Per-peer state

`usePeerState('cursor')` returns `{ [peerId]: value }` with each other member's latest value, and a setter for
ours. Use it for things that change many times a second, like a pointer, a player's position or a phone used as a
controller. Values go on the unreliable lane (latest wins), then once on the reliable lane when they have been
still for 250 ms, so the final position always arrives. A member that leaves drops out of the record; one that
joins gets everyone's current value. A value over one datagram (about 1 KB) goes reliably instead.

### Limits

- Members are trusted alike, as in the room itself: any member can write any shared key. A check keeps out values
  of the wrong type, not a member who writes well-typed nonsense.
- Each store keeps at most 1024 shared keys and 1024 per-peer keys (`MAX_KEYS`), up to 256 characters each.
  Keys past that from other members are ignored.
- A value is at most 64 KiB as JSON (`MAX_VALUE_LENGTH`): setting a larger one throws a `RangeError`, and a
  larger one from a member is ignored. For more, split it across keys. A store holds at most 16 MiB of values in
  all (`MAX_STORE_LENGTH`); past that, members' new values are ignored. A member that leaves frees its per-peer
  values; shared values stay.
- A store answers at most 4 catch-up requests from a member while it stays, so no member can make it send its
  whole state over and over.
- Shared and per-peer state travel as messages with a `$peerwire` field. The hooks hide them, but code that calls
  `room.on('message')` directly sees them.
- `useMessages` holds messages from when it mounted. It does not fetch history: a room has none.

## Next.js and server rendering

The package is marked `'use client'`, and rooms are joined in effects, so components using it render on a
server (without a room) and join in the browser. Importing it touches no browser globals.

## Without the provider

```tsx
const { room, status } = usePeerwireRoom(ready ? { appId: 'my-app', roomId } : null);
const peers = usePeers({ room });
```

## Example and tests

- `example/`: a Vite + React + TypeScript app with presence, members, a shared counter, chat and live cursors.
  Run `npx vite example` in this directory and open the printed URL in two tabs. Add `&signal=nostr` to reach
  other devices.
- `npm run build`: `tsc` to `dist/` (JS and `.d.ts`). `npm pack` builds first.
- `npm run typecheck`: the package, the example and `test/types.tsx`, whose `@ts-expect-error` lines must stay
  errors.
- `node test/hooks.mjs`: the hooks in Node, in react-dom over jsdom, with members of one room over
  BroadcastChannel. It covers server rendering, StrictMode with no ghost member, presence both ways, messages and their limit, shared
  state (concurrent writes converge, a late joiner catches up), per-peer values, leaving, switching rooms, a
  failed join, a member that sends malformed or flooding state messages, the value and memory caps, the limit on
  catch-up answers, and `accept` keeping a member's wrongly typed values out. Needs `packages/js/build.sh` first.
- `node test/example.mjs [screenshot-prefix]`: builds the example with Vite and plays it in three tabs of
  headless Chromium on loopback, with a fourth member outside React that sends the wrong types.
