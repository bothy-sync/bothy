import 'package:bothy/bothy.dart';
import 'package:flutter/material.dart';

import 'app_state.dart';
import 'background.dart';
import 'screens/notes.dart';
import 'screens/password.dart';
import 'screens/sign_in.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await Bothy.initialize();
  await scheduleBackgroundSync();
  final state = AppState();
  runApp(NotesApp(state: state));
  await state.start();
}

class NotesApp extends StatelessWidget {
  const NotesApp({super.key, required this.state});

  final AppState state;

  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'Bothy Notes',
    theme: ThemeData(colorSchemeSeed: Colors.teal, useMaterial3: true),
    darkTheme: ThemeData(
      colorSchemeSeed: Colors.teal,
      brightness: Brightness.dark,
      useMaterial3: true,
    ),
    home: ListenableBuilder(
      listenable: state,
      builder: (_, _) => switch (state.stage) {
        Stage.loading => const Scaffold(
          body: Center(child: CircularProgressIndicator()),
        ),
        Stage.signIn => SignInScreen(state: state),
        Stage.password => PasswordScreen(state: state),
        Stage.notes => NotesScreen(key: ValueKey(state.db), state: state),
      },
    ),
  );
}
