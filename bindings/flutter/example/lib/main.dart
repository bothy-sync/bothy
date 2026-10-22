import 'package:bothy/bothy.dart';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await Bothy.initialize();
  final dir = await getApplicationSupportDirectory();
  final db = await Bothy.open(
    path: '${dir.path}/example.db',
    appId: 'dev.bothy.example',
    // Another copy of the app pointed at the same folder sees the same counter.
    remote: BothyRemoteSpec.path('${dir.path}/shared'),
  );
  await db.initOrJoin();
  runApp(MaterialApp(home: CounterPage(db: db)));
}

class CounterPage extends StatefulWidget {
  const CounterPage({super.key, required this.db});

  final Bothy db;

  @override
  State<CounterPage> createState() => _CounterPageState();
}

class _CounterPageState extends State<CounterPage> {
  late final counters = widget.db.collection('counters');
  int _count = 0;

  @override
  void initState() {
    super.initState();
    counters.watch().listen((_) => _load());
    widget.db.startAutoSync();
    _load();
  }

  Future<void> _load() async {
    final doc = await counters.get('main');
    if (mounted) setState(() => _count = (doc?['count'] as int?) ?? 0);
  }

  Future<void> _increment() => counters.put('main', {'count': _count + 1});

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: const Text('bothy example')),
    body: Center(
      child: Text('$_count', style: Theme.of(context).textTheme.displayLarge),
    ),
    floatingActionButton: FloatingActionButton(
      onPressed: _increment,
      child: const Icon(Icons.add),
    ),
  );
}
