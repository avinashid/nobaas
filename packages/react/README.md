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

export function App() {
  return (
    <RoomProvider appId="my-app" roomId="lobby" presence={{ name: 'Ana' }}>
      <Counter />
    </RoomProvider>
  );
}

function Counter() {
  const peers = usePeers<{ name: string }>();
  const [count, setCount] = useSharedState('count', 0);
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
| `usePeers<P>()` | The other members, oldest first: `{ id, presence }[]`. |
| `usePresence<P>()` | `[ours, setOurs]`, like `useState`; every member sees the change. |
| `useMessages<T>(limit = 200)` | `{ messages, send, clear }`. `messages` holds the newest `limit`, each `{ id, peer, data, lane, kind, at, mine }`; `send(data, { to, lane })` is `room.send`, and adds the message with `mine: true`. |
| `useRoomEvent(event, handler)` | Calls the latest `handler` for `'peer-join'`, `'peer-leave'`, `'presence'` or `'message'`. |
| `useSharedState<T>(key, initial)` | `[value, setValue]` shared by the whole room (below). |
| `usePeerState<T>(key)` | `[byPeer, setOurs]`: each other member's latest value, such as a cursor (below). |
| `usePeerwireRoom(options \| null)` | What `RoomProvider` uses: joins without a provider, returns `{ room, status, error }`; null stays out. |

Every hook but `useRoom` takes an optional last argument, a `Room` (or null), to work on that room instead of the
nearest provider's. `randomRoomId` and the `@peerwire/js` types (`Room`, `PeerId`, `JoinRoomOptions`, ...) are
re-exported.

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

- Members are trusted alike, as in the room itself: any member can write any shared key. Data from another member
  is untrusted input to your app, whatever the TypeScript type says, so check it before you use it.
- Each store keeps at most 1024 shared keys and 1024 per-peer keys (`MAX_KEYS`), up to 256 characters each.
  Keys past that from other members are ignored.
- Shared and per-peer state travel as messages with a `$peerwire` field. The hooks hide them, but code that calls
  `room.on('message')` directly sees them.
- `useMessages` holds messages from when it mounted. It does not fetch history: a room has none.

## Next.js and server rendering

The package is marked `'use client'`, and rooms are joined in effects, so components using it render on a
server (without a room) and join in the browser. Importing it touches no browser globals.

## Without the provider

```tsx
const { room, status } = usePeerwireRoom(ready ? { appId: 'my-app', roomId } : null);
const peers = usePeers(room);
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
  failed join, and a member that sends malformed or flooding state messages. Needs `packages/js/build.sh` first.
- `node test/example.mjs [screenshot-prefix]`: builds the example with Vite and plays it in three tabs of
  headless Chromium on loopback.
