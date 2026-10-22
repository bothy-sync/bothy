import 'package:bothy/bothy.dart';
import 'package:flutter/material.dart';

/// Edits from other devices that lost to a newer edit of the same field.
class ConflictsScreen extends StatefulWidget {
  const ConflictsScreen({super.key, required this.db});

  final Bothy db;

  @override
  State<ConflictsScreen> createState() => _ConflictsScreenState();
}

class _ConflictsScreenState extends State<ConflictsScreen> {
  List<BothyConflict> _rows = [];

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final rows = await widget.db.conflicts();
    if (mounted) setState(() => _rows = rows);
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(
      title: const Text('Conflicts'),
      actions: [
        TextButton(
          onPressed: _rows.isEmpty
              ? null
              : () async {
                  await widget.db.clearConflicts();
                  await _load();
                },
          child: const Text('Clear'),
        ),
      ],
    ),
    body: _rows.isEmpty
        ? const Center(child: Text('No lost edits'))
        : ListView.builder(
            itemCount: _rows.length,
            itemBuilder: (_, i) {
              final c = _rows[i];
              final when = DateTime.fromMillisecondsSinceEpoch(c.atMs);
              return ListTile(
                title: Text('${c.id} · ${c.field}'),
                subtitle: Text(
                  'Lost value: ${c.loserValueJson ?? '(removed)'}\n'
                  'From device ${c.loserDevice.substring(0, 8)}, '
                  'replaced by ${c.winnerDevice.substring(0, 8)} · $when',
                ),
                isThreeLine: true,
              );
            },
          ),
  );
}
