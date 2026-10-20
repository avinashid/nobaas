extends Node2D
## Nobaas in Godot, no server: one instance hosts, the others join with its invite.
## Arrow keys move your square (MultiplayerSynchronizer); the text box chats (@rpc).
##
## Headless check (see test.sh): `-- --host=FILE` writes the invite to FILE,
## `-- --join=FILE` reads it. Both move by themselves, chat, and quit with 0 once
## they have seen another peer's square move and its chat line, or 1 on timeout.

const Player := preload("res://player.gd")
const TIMEOUT := 20.0

var peer := NobaasMultiplayerPeer.new()
var players := Node2D.new()
var spawner := MultiplayerSpawner.new()
var invite_edit := LineEdit.new()
var chat_edit := LineEdit.new()
var log_label := Label.new()
var args := {}
var headless := false
var got_chat := false
var done_at := -1.0
var elapsed := 0.0


func _ready() -> void:
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
	multiplayer.connection_failed.connect(func(): _log("could not reach the host"))
	multiplayer.server_disconnected.connect(func(): _log("the host left"))
	for a in OS.get_cmdline_user_args():
		var kv := a.trim_prefix("--").split("=", true, 1)
		args[kv[0]] = kv[1] if kv.size() > 1 else ""
	headless = args.has("host") or args.has("join")
	if args.has("host"):
		_host()
	elif args.has("join"):
		invite_edit.text = FileAccess.get_file_as_string(args["join"]).strip_edges()
		_join()


func _host() -> void:
	var invite := peer.host()
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
	_log("hosting as peer 1; invite copied to the clipboard")
	spawner.spawn(1)


func _join() -> void:
	if peer.join(invite_edit.text.strip_edges()) != OK:
		_log("that is not an nb: invite")
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


@rpc("any_peer", "call_local", "reliable")
func chat(text: String) -> void:
	var from := multiplayer.get_remote_sender_id()
	_log("[%d] %s" % [from, text])
	if from != multiplayer.get_unique_id():
		got_chat = true


func _process(delta: float) -> void:
	if not headless:
		return
	elapsed += delta
	if done_at < 0.0 and got_chat and _other_square_moved():
		print("NOBAAS_DEMO_OK peer=%d rtt_ms=%.1f" % [multiplayer.get_unique_id(), _rtt()])
		done_at = elapsed
	# The host lingers a moment so the joiner can finish too (`--linger=SECONDS` to stay longer).
	var linger := float(args.get("linger", 2.0 if multiplayer.is_server() else 0.2))
	if done_at >= 0.0 and elapsed - done_at > linger:
		get_tree().quit(0)
	elif done_at < 0.0 and elapsed > TIMEOUT:
		print("NOBAAS_DEMO_TIMEOUT chat=%s moved=%s" % [got_chat, _other_square_moved()])
		get_tree().quit(1)


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
	invite_edit.placeholder_text = "paste an nb: invite, then Join"
	invite_edit.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	row.add_child(invite_edit)
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
