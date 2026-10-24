import { useState } from 'react';
import type { FormEvent, PointerEvent } from 'react';
import { RoomProvider, useMessages, usePeers, usePeerState, useRoom, useSharedState } from '@peerwire/react';

type Me = { name: string };
type Point = { x: number; y: number };

export function App({ roomId, signaling, initialName }: { roomId: string; signaling: 'local' | 'nostr'; initialName: string }) {
  const [name, setName] = useState(initialName);
  return (
    <RoomProvider appId="peerwire-react-example" roomId={roomId} signaling={signaling} presence={{ name } satisfies Me}>
      <header>
        <label>
          Name <input id="name" value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <Status />
      </header>
      <main>
        <Members />
        <Counter />
        <Chat />
        <Board />
      </main>
    </RoomProvider>
  );
}

function Status() {
  const { room, status, error } = useRoom();
  if (status === 'error') return <span id="status">Couldn't join: {error?.message}</span>;
  return <span id="status">{room ? `joined as ${room.self.slice(0, 6)}` : 'joining…'}</span>;
}

function Members() {
  const peers = usePeers<Me>();
  return (
    <section>
      <h2>In the room ({peers.length} besides you)</h2>
      <ul id="members">
        {peers.map((p) => (
          <li key={p.id} data-name={p.presence.name}>
            {p.presence.name}
          </li>
        ))}
      </ul>
    </section>
  );
}

// One number for the whole room: anyone can change it, and someone who joins later sees it.
function Counter() {
  const [count, setCount] = useSharedState('count', 0);
  return (
    <section>
      <h2>Shared counter</h2>
      <output id="count">{count}</output>
      <button onClick={() => setCount((c) => c - 1)}>−1</button> <button onClick={() => setCount((c) => c + 1)}>+1</button>{' '}
      <button onClick={() => setCount(0)}>Reset</button>
    </section>
  );
}

function Chat() {
  const peers = usePeers<Me>();
  const { messages, send } = useMessages<string>(100);
  const [text, setText] = useState('');
  const nameOf = (id: string) => peers.find((p) => p.id === id)?.presence.name ?? id.slice(0, 6);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (text.trim()) send(text.trim());
    setText('');
  };
  return (
    <section>
      <h2>Chat</h2>
      <ul id="log">
        {messages.map((m) => (
          <li key={m.id}>
            <b>{m.mine ? 'me' : nameOf(m.peer)}:</b> {String(m.data)}
          </li>
        ))}
      </ul>
      <form id="chat" onSubmit={submit}>
        <input value={text} onChange={(e) => setText(e.target.value)} placeholder="Say something" />
        <button>Send</button>
      </form>
    </section>
  );
}

// Everyone's pointer, live: positions go on the unreliable lane, as fractions of the board.
function Board() {
  const peers = usePeers<Me>();
  const [cursors, setCursor] = usePeerState<Point>('cursor');
  const move = (e: PointerEvent<HTMLDivElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    setCursor({ x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height });
  };
  return (
    <section>
      <h2>Cursors</h2>
      <div id="board" onPointerMove={move}>
        {peers.map((p) => {
          const c = cursors[p.id];
          return c ? (
            <span key={p.id} className="cursor" data-name={p.presence.name} style={{ left: `${c.x * 100}%`, top: `${c.y * 100}%` }}>
              {p.presence.name}
            </span>
          ) : null;
        })}
      </div>
    </section>
  );
}
