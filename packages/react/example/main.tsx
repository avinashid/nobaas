import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { randomRoomId } from '@peerwire/react';
import { App } from './App';

const params = new URLSearchParams(location.search);
// The room is shared as a link (?room=<id>). A new one is random, so only people given the link can join.
let roomId = params.get('room');
if (!roomId) {
  roomId = randomRoomId();
  params.set('room', roomId);
  history.replaceState(null, '', `?${params}`);
}
// 'local': tabs of this browser. ?signal=nostr: any device, through public Nostr relays.
const signaling = params.get('signal') === 'nostr' ? 'nostr' : 'local';
const name = params.get('name') ?? `guest-${Math.floor(Math.random() * 1000)}`;

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App roomId={roomId} signaling={signaling} initialName={name} />
  </StrictMode>,
);
