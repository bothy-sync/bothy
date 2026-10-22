import 'package:flutter/material.dart';

import '../app_state.dart';

class PasswordScreen extends StatefulWidget {
  const PasswordScreen({super.key, required this.state});

  final AppState state;

  @override
  State<PasswordScreen> createState() => _PasswordScreenState();
}

class _PasswordScreenState extends State<PasswordScreen> {
  final _password = TextEditingController();
  bool _remember = true;
  bool _busy = false;

  Future<void> _submit() async {
    if (_password.text.isEmpty) return;
    setState(() => _busy = true);
    await widget.state.submitPassword(_password.text, remember: _remember);
    if (mounted) setState(() => _busy = false);
  }

  @override
  void dispose() {
    _password.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final first = widget.state.firstSetup;
    return Scaffold(
      appBar: AppBar(
        title: Text(first ? 'Set up' : 'Unlock'),
        actions: [
          IconButton(
            tooltip: 'Sign out',
            onPressed: widget.state.signOut,
            icon: const Icon(Icons.logout),
          ),
        ],
      ),
      body: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 400),
          child: Padding(
            padding: const EdgeInsets.all(24),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Text(
                  first
                      ? 'Enter the password of your notes. On your first device this '
                            'creates an encrypted database; on the others it joins it. '
                            'There is no way to recover a forgotten password.'
                      : 'Enter your password to sync again.',
                ),
                const SizedBox(height: 16),
                TextField(
                  controller: _password,
                  obscureText: true,
                  autofocus: true,
                  decoration: const InputDecoration(labelText: 'Password'),
                  onSubmitted: (_) => _submit(),
                ),
                CheckboxListTile(
                  value: _remember,
                  onChanged: (v) => setState(() => _remember = v ?? false),
                  title: const Text('Remember on this device'),
                  contentPadding: EdgeInsets.zero,
                  controlAffinity: ListTileControlAffinity.leading,
                ),
                if (widget.state.error != null)
                  Padding(
                    padding: const EdgeInsets.only(bottom: 12),
                    child: Text(
                      widget.state.error!,
                      style: TextStyle(
                        color: Theme.of(context).colorScheme.error,
                      ),
                    ),
                  ),
                FilledButton(
                  onPressed: _busy ? null : _submit,
                  child: _busy
                      ? const SizedBox.square(
                          dimension: 18,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        )
                      : Text(first ? 'Continue' : 'Unlock'),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
