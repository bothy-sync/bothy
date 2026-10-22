import 'package:bothy/bothy.dart';
import 'package:flutter/material.dart';

class EditorScreen extends StatefulWidget {
  const EditorScreen({super.key, required this.notes, this.doc});

  final BothyCollection notes;

  /// `null` for a new note.
  final BothyDocument? doc;

  @override
  State<EditorScreen> createState() => _EditorScreenState();
}

class _EditorScreenState extends State<EditorScreen> {
  late final String _id =
      widget.doc?.id ?? 'n${DateTime.now().microsecondsSinceEpoch}';
  late final _title = TextEditingController(
    text: widget.doc?.data['title'] as String?,
  );
  late final _body = TextEditingController(
    text: widget.doc?.data['body'] as String?,
  );
  bool _deleted = false;

  /// Writes only the fields that changed, so an edit to the body on one device and to the
  /// title on another both survive.
  Future<void> _save() async {
    if (_deleted) return;
    final old = widget.doc?.data ?? const {};
    final changed = <String, Object?>{
      if (_title.text != (old['title'] ?? '')) 'title': _title.text,
      if (_body.text != (old['body'] ?? '')) 'body': _body.text,
    };
    if (changed.isEmpty) return;
    if (widget.doc == null && _title.text.isEmpty && _body.text.isEmpty) return;
    changed['updated_ms'] = DateTime.now().millisecondsSinceEpoch;
    await widget.notes.patch(_id, changed);
  }

  @override
  void dispose() {
    _title.dispose();
    _body.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => PopScope(
    onPopInvokedWithResult: (didPop, _) => _save(),
    child: Scaffold(
      appBar: AppBar(
        actions: [
          if (widget.doc != null)
            IconButton(
              tooltip: 'Delete',
              icon: const Icon(Icons.delete_outline),
              onPressed: () async {
                _deleted = true;
                await widget.notes.delete(_id);
                if (context.mounted) Navigator.of(context).pop();
              },
            ),
        ],
      ),
      body: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16),
        child: Column(
          children: [
            TextField(
              controller: _title,
              style: Theme.of(context).textTheme.titleLarge,
              decoration: const InputDecoration(
                hintText: 'Title',
                border: InputBorder.none,
              ),
            ),
            Expanded(
              child: TextField(
                controller: _body,
                maxLines: null,
                expands: true,
                decoration: const InputDecoration(
                  hintText: 'Note',
                  border: InputBorder.none,
                ),
              ),
            ),
          ],
        ),
      ),
    ),
  );
}
