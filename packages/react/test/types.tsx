// Compile-time checks of the public types: `npm run typecheck` (never run). Each @ts-expect-error must stay an
// error, or tsc fails.
import { RoomProvider, useMessages, usePeers, usePeerState, usePeerwireRoom, usePresence, useRoom, useRoomEvent, useSharedState } from '@peerwire/react';
import type { Message, Peer, PeerId, Room, RoomState, RoomStatus, SetState } from '@peerwire/react';

type Me = { name: string };
type Point = { x: number; y: number };
const isMe = (p: unknown): p is Me => typeof (p as Me)?.name === 'string';
const isPoint = (v: unknown): v is Point => typeof (v as Point)?.x === 'number' && typeof (v as Point)?.y === 'number';

export function Types() {
  const state: RoomState = useRoom();
  const status: RoomStatus = state.status;
  const room: Room | null = state.room;
  room?.send({ any: 'json' }, { lane: 'unreliable' });

  const peers: readonly Peer<Me>[] = usePeers<Me>();
  const first: string | undefined = peers[0]?.presence.name;
  // accept's guard gives the type.
  const checked: readonly Peer<Me>[] = usePeers({ accept: isMe });
  // @ts-expect-error: inferred, not any
  const notAny1: number = checked[0]!.presence.name;

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
  const [points] = usePeerState('cursor', { accept: isPoint });
  const p: Point | undefined = points['ab' as PeerId];
  // @ts-expect-error: inferred, not any
  const notAny2: string = points['ab' as PeerId]!;
  const [flag] = useSharedState('flag', false, { accept: (v) => typeof v === 'boolean' });
  const on: boolean = flag;
  // @ts-expect-error: inferred, not any
  const notAny4: string = flag;
  const at: Point | undefined = cursors['ab' as PeerId];
  setCursor({ x: 1, y: 2 });
  // @ts-expect-error: y is missing
  setCursor({ x: 1 });

  const { messages, send } = useMessages<string>({ limit: 50 });
  const { messages: texts } = useMessages({ accept: (d) => typeof d === 'string' });
  const text: string | undefined = texts[0]?.data;
  // @ts-expect-error: inferred, not any
  const notAny3: number = texts[0]!.data;
  // @ts-expect-error: the limit goes in the options
  useMessages(50);
  const last: Message<string> | undefined = messages.at(-1);
  const sentTo: number = send('hi', { to: peers.map((p) => p.id) });
  // @ts-expect-error: a string list
  send(42);

  useRoomEvent('message', (data, peer: PeerId, info) => void [data, peer, info.lane satisfies 'reliable' | 'unreliable']);
  useRoomEvent('peer-leave', (peer, reason) => void [peer, reason satisfies string]);
  // @ts-expect-error: no such event
  useRoomEvent('nope', () => {});

  const explicit = usePeerwireRoom(null);
  usePeers({ room: explicit.room });
  useRoomEvent('presence', () => {}, { room: explicit.room });

  return (
    <>
      <RoomProvider appId="a" roomId="r" presence={{ name: 'ana' }} signaling="nostr">
        {String([status, first, checked, myName, count, at, p, on, last, text, sentTo, notAny1, notAny2, notAny3, notAny4])}
      </RoomProvider>
      {/* @ts-expect-error: appId is required */}
      <RoomProvider roomId="r" />
    </>
  );
}
