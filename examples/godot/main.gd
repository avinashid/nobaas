extends Node2D
## Peerwire in Godot, no server: one instance hosts, the others join with its invite
## or find it by room name, through NATs. Arrow keys move your square
## (MultiplayerSynchronizer); the text box chats (@rpc).
##
## Headless check (see test.sh): `-- --host=FILE` writes the invite to FILE,
## `-- --join=FILE` reads it; `-- --host-room=ROOM` / `-- --join-room=ROOM` meet by
## room instead. `--stun=a:3478,b:3478` and `--relay=wss://…,…` set the STUN servers
## and Nostr relays (`none` turns either off); `--privacy=public-only|relay-only`
## which addresses this peer gives out; `--port-mapping` asks the router to forward
## a port (off by default; the state line shows how it went); `--relay-peer=pw:…[,…]`
## keeps a link to always-on relay peers (`peerwire-peer`), through which two
## players link when their NATs can't be punched. The log says whether each link
## is direct (and to which address, and whether a birthday punch made it) or
## through a relay. Headless, both move by themselves, chat, and
## quit with 0 once they have seen another peer's square move and its chat line,
## or 1 on timeout.

const Player := preload("res://player.gd")
const TIMEOUT := 20.0
## Rooms are scoped to an application: pick a name of your own.
const APP_ID := "peerwire-godot-demo"

var peer := PeerwireMultiplayerPeer.new()
var players := Node2D.new()
var spawner := MultiplayerSpawner.new()
var invite_edit := LineEdit.new()
var room_edit := LineEdit.new()
var chat_edit := LineEdit.new()
var log_label := Label.new()
var args := {}
var headless := false
var got_chat := false
var done_at := -1.0
var elapsed := 0.0
var nat_logged := false
var relays_linked := -1
## Godot peer id -> its route line, as last logged.
var routes := {}
## Birthday punch sockets this side has opened, as last logged.
var birthday_sockets := 0
## Whether this side's birthday probing has been logged.
var probing := false


func _ready() -> void:
	# Peers are untrusted: never let them ship encoded Objects (Godot's default, asserted).
	assert(not (multiplayer as SceneMultiplayer).allow_object_decoding)
	players.name = "Players"
	add_child(players)
	spawner.name = "Spawner"
	spawner.spawn_function = _spawn_player
	add_child(spawner)
	spawner.spawn_path = spawner.get_path_to(players)
	_build_ui()
	multiplayer.peer_connected.connect(_on_peer_connected)
	multiplayer.peer_disconnected.connect(_on_peer_disconnected)
	multiplayer.connected_to_server.connect(_on_connected_to_server)
	multiplayer.connection_failed.connect(func(): _log("could not reach the host: %s" % peer.get_connect_error_message()))
	multiplayer.server_disconnected.connect(func(): _log("the host left"))
	for a in OS.get_cmdline_user_args():
		var kv := a.trim_prefix("--").split("=", true, 1)
		args[kv[0]] = kv[1] if kv.size() > 1 else ""
	if args.has("stun"):
		peer.set_stun_servers(_list(args["stun"]))
	if args.has("relay"):
		peer.set_signaling_relays(_list(args["relay"]))
	if args.has("privacy"):
		var modes := {"direct": PeerwireMultiplayerPeer.PRIVACY_DIRECT,
			"public-only": PeerwireMultiplayerPeer.PRIVACY_PUBLIC_ONLY,
			"relay-only": PeerwireMultiplayerPeer.PRIVACY_RELAY_ONLY}
		peer.set_privacy_mode(modes[args["privacy"]])
		assert(peer.get_privacy_mode() == modes[args["privacy"]])
		_log("privacy: %s" % args["privacy"])
	if args.has("port-mapping"):
		peer.set_port_mapping(true)
	if args.has("relay-peer"):
		if peer.set_relay_peers(_list(args["relay-peer"])) != OK:
			_log("--relay-peer: not a pw: invite")
	peer.port_mapping_changed.connect(func(info: Dictionary): _log("port mapping: %s" % _mapping_line(info)))
	headless = args.has("host") or args.has("join") or args.has("host-room") or args.has("join-room")
	if args.has("host"):
		_host()
	elif args.has("host-room"):
		room_edit.text = args["host-room"]
		_host_room()
	elif args.has("join"):
		invite_edit.text = FileAccess.get_file_as_string(args["join"]).strip_edges()
		_join()
	elif args.has("join-room"):
		room_edit.text = args["join-room"]
		_join_room()


func _list(value: String) -> PackedStringArray:
	return PackedStringArray() if value == "none" else value.split(",", false)


func _host() -> void:
	_start_hosting(peer.host(), "hosting as peer 1; invite copied to the clipboard")


func _host_room() -> void:
	var room := room_edit.text.strip_edges()
	if room.is_empty():
		_log("type a room name first")
		return
	_start_hosting(peer.host_room(APP_ID, room), "hosting room '%s' as peer 1" % room)


func _join_room() -> void:
	var room := room_edit.text.strip_edges()
	if room.is_empty():
		_log("type a room name first")
		return
	if peer.join_room(APP_ID, room) != OK:
		_log("could not open a UDP port")
		return
	multiplayer.multiplayer_peer = peer
	_log("looking for the host of room '%s'…" % room)


func _start_hosting(invite: String, message: String) -> void:
	if invite.is_empty():
		_log("could not open a UDP port")
		return
	multiplayer.multiplayer_peer = peer
	invite_edit.text = invite
	if DisplayServer.get_name() != "headless":
		DisplayServer.clipboard_set(invite)
	if args.has("host"):
		var f := FileAccess.open(args["host"], FileAccess.WRITE)
		f.store_string(invite)
		f.close()
	_log(message)
	_log("port mapping: %s" % _mapping_line(peer.get_port_mapping()))
	spawner.spawn(1)


## "off", "trying", "mapped 203.0.113.7:41000 (upnp)", "not_public …", "unavailable" or "relay_only".
func _mapping_line(info: Dictionary) -> String:
	if info["address"].is_empty():
		return info["state"]
	return "%s %s (%s)" % [info["state"], info["address"], info["protocol"]]


func _join() -> void:
	if peer.join(invite_edit.text.strip_edges()) != OK:
		_log("that is not an pw: invite")
		return
	multiplayer.multiplayer_peer = peer
	_log("joining…")


func _on_peer_connected(id: int) -> void:
	if multiplayer.is_server():
		_log("peer %d joined" % id)
		spawner.spawn(id)
		chat.rpc("welcome, %d" % id)


func _on_connected_to_server() -> void:
	_log("joined as peer %d" % multiplayer.get_unique_id())
	chat.rpc("hello from %d" % multiplayer.get_unique_id())


func _on_peer_disconnected(id: int) -> void:
	_log("peer %d left" % id)
	if multiplayer.is_server():
		var p := players.get_node_or_null(str(id))
		if p:
			p.queue_free()


func _spawn_player(id: int) -> Node:
	var p := Player.new()
	p.name = str(id)
	p.start = Vector2(60 + (id % 7) * 100, 240)
	p.position = p.start
	p.auto = headless
	return p


## Any peer can call this with any argument: check it before use.
@rpc("any_peer", "call_local", "reliable")
func chat(text: String) -> void:
	var from := multiplayer.get_remote_sender_id()
	text = text.strip_edges().left(200)
	if text.is_empty():
		return
	_log("[%d] %s" % [from, text])
	if from != multiplayer.get_unique_id():
		got_chat = true


func _process(delta: float) -> void:
	if not nat_logged and multiplayer.has_multiplayer_peer() and peer.get_nat_type() != "unknown":
		nat_logged = true
		_log("public address %s, NAT %s" % [peer.get_public_address(), peer.get_nat_type()])
	_log_links()
	if not headless:
		return
	elapsed += delta
	if done_at < 0.0 and got_chat and _other_square_moved():
		print("PEERWIRE_DEMO_OK peer=%d rtt_ms=%.1f" % [multiplayer.get_unique_id(), _rtt()])
		done_at = elapsed
	# The host lingers a moment so the joiner can finish too (`--linger=SECONDS` to stay longer).
	var linger := float(args.get("linger", 2.0 if multiplayer.is_server() else 0.2))
	if done_at >= 0.0 and elapsed - done_at > linger:
		get_tree().quit(0)
	elif done_at < 0.0 and elapsed > TIMEOUT:
		print("PEERWIRE_DEMO_TIMEOUT chat=%s moved=%s" % [got_chat, _other_square_moved()])
		get_tree().quit(1)


## Logs how many relay peers are linked, each link's route (and whether a birthday
## punch made it), and a birthday punch starting on this side, when they change.
func _log_links() -> void:
	if not multiplayer.has_multiplayer_peer():
		return
	if args.has("relay-peer"):
		var linked := peer.get_relay_peers_linked()
		if linked != relays_linked:
			relays_linked = linked
			_log("relay peers: %d linked" % linked)
	var stats := peer.get_punch_stats()
	if stats["birthday_sockets"] != birthday_sockets:
		birthday_sockets = stats["birthday_sockets"]
		_log("birthday punch: this side opened %d sockets for the peer to find" % birthday_sockets)
	if not probing and stats["birthday_probes"] > 0:
		probing = true
		_log("birthday punch: this side is probing the peer's ports")
	for id in multiplayer.get_peers():
		var line := _route_line(id, stats)
		if not line.is_empty() and routes.get(id, "") != line:
			routes[id] = line
			_log("route to peer %d: %s" % [id, line])


## "through a relay", "direct to ADDRESS" and, if a birthday punch made the link,
## which side's part of it shows here; "" if `id` isn't linked.
func _route_line(id: int, stats: Dictionary) -> String:
	match peer.get_peer_route(id):
		"relay":
			return "through a relay"
		"direct":
			var line := "direct to %s" % peer.get_peer_address(id)
			if peer.is_peer_via_birthday(id):
				return line + " (birthday punch worked: over one of this side's birthday sockets)"
			if stats["birthday_probes"] > 0:
				return line + " (this side probed for a birthday punch)"
			return line
	return ""


func _other_square_moved() -> bool:
	for p in players.get_children():
		if p.peer_id != multiplayer.get_unique_id() and p.position.distance_to(p.start) > 5.0:
			return true
	return false


## RTT to the host, or (on the host) to the first joiner. Other joiners are relayed.
func _rtt() -> float:
	if multiplayer.is_server():
		var ids := multiplayer.get_peers()
		return peer.get_peer_rtt_ms(ids[0]) if ids.size() > 0 else -1.0
	return peer.get_peer_rtt_ms(1)


func _build_ui() -> void:
	var layer := CanvasLayer.new()
	add_child(layer)
	var box := VBoxContainer.new()
	box.position = Vector2(10, 10)
	box.custom_minimum_size = Vector2(780, 0)
	layer.add_child(box)
	var row := HBoxContainer.new()
	box.add_child(row)
	var host_button := Button.new()
	host_button.text = "Host"
	host_button.pressed.connect(_host)
	row.add_child(host_button)
	var join_button := Button.new()
	join_button.text = "Join"
	join_button.pressed.connect(_join)
	row.add_child(join_button)
	invite_edit.placeholder_text = "paste an pw: invite, then Join"
	invite_edit.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	row.add_child(invite_edit)
	var room_row := HBoxContainer.new()
	box.add_child(room_row)
	var host_room_button := Button.new()
	host_room_button.text = "Host room"
	host_room_button.pressed.connect(_host_room)
	room_row.add_child(host_room_button)
	var join_room_button := Button.new()
	join_room_button.text = "Join room"
	join_room_button.pressed.connect(_join_room)
	room_row.add_child(join_room_button)
	room_edit.placeholder_text = "or a room name: everyone typing it meets, across NATs"
	room_edit.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	room_row.add_child(room_edit)
	chat_edit.placeholder_text = "say something, Enter to send"
	chat_edit.text_submitted.connect(func(text: String):
		if multiplayer.has_multiplayer_peer() and not text.is_empty():
			chat.rpc(text)
		chat_edit.clear())
	box.add_child(chat_edit)
	box.add_child(log_label)


func _log(line: String) -> void:
	print(line)
	var lines := (log_label.text + "\n" + line).strip_edges().split("\n")
	log_label.text = "\n".join(lines.slice(max(0, lines.size() - 8)))
