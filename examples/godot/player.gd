extends Node2D
## A square owned by one peer. Its MultiplayerSynchronizer copies the owner's
## position to everyone else.

const SIZE := 40.0
const SPEED := 260.0

var peer_id := 1
var start := Vector2.ZERO
## Move in a circle instead of reading the keyboard (headless runs).
var auto := false
var _t := 0.0


func _init() -> void:
	# The synchronizer must be a child before the square enters the tree.
	var sync := MultiplayerSynchronizer.new()
	sync.name = "Sync"
	var config := SceneReplicationConfig.new()
	config.add_property(NodePath(".:position"))
	sync.replication_config = config
	add_child(sync)


func _enter_tree() -> void:
	peer_id = name.to_int()
	set_multiplayer_authority(peer_id) # recursive: the synchronizer follows


func _process(delta: float) -> void:
	if is_multiplayer_authority():
		var dir := Input.get_vector("ui_left", "ui_right", "ui_up", "ui_down")
		if auto:
			_t += delta
			dir = Vector2(cos(_t * 2.0), sin(_t * 2.0))
		position += dir * SPEED * delta
		position = position.clamp(Vector2.ZERO, get_viewport_rect().size - Vector2(SIZE, SIZE))
	queue_redraw()


func _draw() -> void:
	var color := Color.from_hsv(fmod(peer_id * 0.618034, 1.0), 0.6, 0.95)
	draw_rect(Rect2(Vector2.ZERO, Vector2(SIZE, SIZE)), color)
	var label := "you" if is_multiplayer_authority() else str(peer_id)
	draw_string(ThemeDB.fallback_font, Vector2(0, SIZE + 16), label)
