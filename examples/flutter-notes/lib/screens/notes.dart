import 'dart:async';

import 'package:bothy/bothy.dart';
import 'package:flutter/material.dart';

import '../app_state.dart';
import 'conflicts.dart';
import 'editor.dart';

class NotesScreen extends StatefulWidget {
  const NotesScreen({super.key, required this.state});

  final AppState state;

  @override
  State<NotesScreen> createState() => _NotesScreenState();
}

class _NotesScreenState extends State<NotesScreen> {
  late final BothyCollection _notes = widget.state.db!.collection('notes');
  StreamSubscription<BothyChange>? _changes;
  List<BothyDocument> _rows = [];
  int _conflicts = 0;

  @override
  void initState() {
    super.initState();
    _changes = _notes.watch().listen((_) => _load());
    _load();
  }

  @override
  void dispose() {
    _changes?.cancel();
    super.dispose();
  }

  Future<void> _load() async {
    final rows = await _notes.list(limit: 1000);
    rows.sort(
      (a, b) => ((b.data['updated_ms'] as num?) ?? 0).compareTo(
        (a.data['updated_ms'] as num?) ?? 0,
      ),
    );
    final conflicts = (await widget.state.db!.conflicts()).length;
    if (mounted) {
      setState(() {
        _rows = rows;
        _conflicts = conflicts;
      });
    }
  }

  Future<void> _open(BothyDocument? doc) async {
    await Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => EditorScreen(notes: _notes, doc: doc),
      ),
    );
    await _load();
  }

  String _status() {
    final s = widget.state;
    if (s.syncing) return 'Syncing…';
    final e = s.lastSync;
    if (e == null) return 'Not synced yet';
    if (e.error != null) return 'Sync failed: ${e.error!.message}';
    final r = e.report!;
    final t = TimeOfDay.now().format(context);
    return 'Synced at $t · ${r.pushedOps} sent, ${r.pulledOps} received';
  }

  @override
  Widget build(BuildContext context) {
    final state = widget.state;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Notes'),
        actions: [
          IconButton(
            tooltip: 'Conflicts',
            onPressed: () async {
              await Navigator.of(context).push(
                MaterialPageRoute<void>(
                  builder: (_) => ConflictsScreen(db: state.db!),
                ),
              );
              await _load();
            },
            icon: Badge(
              isLabelVisible: _conflicts > 0,
              label: Text('$_conflicts'),
              child: const Icon(Icons.call_split),
            ),
          ),
          IconButton(
            tooltip: 'Sync now',
            onPressed: state.syncing ? null : state.syncNow,
            icon: const Icon(Icons.sync),
          ),
          PopupMenuButton<String>(
            onSelected: (v) => v == 'lock' ? state.lock() : state.signOut(),
            itemBuilder: (_) => const [
              PopupMenuItem(value: 'lock', child: Text('Lock and forget key')),
              PopupMenuItem(value: 'signout', child: Text('Sign out')),
            ],
          ),
        ],
        bottom: PreferredSize(
          preferredSize: const Size.fromHeight(24),
          child: Padding(
            padding: const EdgeInsets.only(bottom: 6),
            child: Text(
              state.error ?? _status(),
              style: Theme.of(context).textTheme.bodySmall,
            ),
          ),
        ),
      ),
      body: _rows.isEmpty
          ? const Center(child: Text('No notes yet'))
          : ListView.separated(
              itemCount: _rows.length,
              separatorBuilder: (_, _) => const Divider(height: 1),
              itemBuilder: (_, i) {
                final d = _rows[i];
                final title = (d.data['title'] as String?) ?? '';
                final body = (d.data['body'] as String?) ?? '';
                return ListTile(
                  title: Text(title.isEmpty ? '(untitled)' : title),
                  subtitle: Text(
                    body,
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                  ),
                  onTap: () => _open(d),
                );
              },
            ),
      floatingActionButton: FloatingActionButton(
        tooltip: 'New note',
        onPressed: () => _open(null),
        child: const Icon(Icons.add),
      ),
    );
  }
}
