// Compile-time checks of the public types: `npm run typecheck` (never run). Each @ts-expect-error must stay an
// error, or tsc fails.
import { RoomProvider, useMessages, usePeers, usePeerState, usePeerwireRoom, usePresence, useRoom, useRoomEvent, useSharedState } from '@peerwire/react';
import type { Message, Peer, PeerId, Room, RoomState, RoomStatus, SetState } from '@peerwire/react';

type Me = { name: string };
type Point = { x: number; y: number };

export function Types() {
  const state: RoomState = useRoom();
  const status: RoomStatus = state.status;
  const room: Room | null = state.room;
  room?.send({ any: 'json' }, { lane: 'unreliable' });

  const peers: readonly Peer<Me>[] = usePeers<Me>();
  const first: string | undefined = peers[0]?.presence.name;

  const [me, setMe] = usePresence<Me>();
  const myName: string | undefined = me?.name;
  setMe({ name: 'ana' });
  setMe((p) => ({ ...p, name: 'bo' }));
  // @ts-expect-error: not a Me
  setMe({ nick: 'x' });

  const [count, setCount]: [number, SetState<number>] = useSharedState('count', 0);
  setCount((c) => c + 1);
  // @ts-expect-error: a number state
  setCount('one');

  const [cursors, setCursor] = usePeerState<Point>('cursor');
  const at: Point | undefined = cursors['ab' as PeerId];
  setCursor({ x: 1, y: 2 });
  // @ts-expect-error: y is missing
  setCursor({ x: 1 });

  const { messages, send } = useMessages<string>(50);
  const last: Message<string> | undefined = messages.at(-1);
  const sentTo: number = send('hi', { to: peers.map((p) => p.id) });
  // @ts-expect-error: a string list
  send(42);

  useRoomEvent('message', (data, peer: PeerId, info) => void [data, peer, info.lane satisfies 'reliable' | 'unreliable']);
  useRoomEvent('peer-leave', (peer, reason) => void [peer, reason satisfies string]);
  // @ts-expect-error: no such event
  useRoomEvent('nope', () => {});

  const explicit = usePeerwireRoom(null);
  usePeers(explicit.room);

  return (
    <>
      <RoomProvider appId="a" roomId="r" presence={{ name: 'ana' }} signaling="nostr">
        {String([status, first, myName, count, at, last, sentTo])}
      </RoomProvider>
      {/* @ts-expect-error: appId is required */}
      <RoomProvider roomId="r" />
    </>
  );
}
