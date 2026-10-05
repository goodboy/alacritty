ALACRITTY-MSG(1)

# NAME

alacritty-msg - Send messages to Alacritty.

# SYNOPSIS

This command communicates with running Alacritty instances through a socket,
making it possible to control Alacritty without directly accessing it.

# OPTIONS

*-s, --socket* _<SOCKET>_

	Path for IPC socket communication.

# MESSAGES

*create-window*

	Create a new window in the same Alacritty process.

	*FLAGS*
		*--print-pid*

			Print the direct PTY child PID as a decimal number followed by a newline
			after window creation. Creation errors are reported on stderr with a
			nonzero exit status. Requires a running Alacritty with PID reply support.
			Long error messages are summarized; full details are in Alacritty's log.

			The PID can identify a shell, command, or platform login helper. It is
			not a window ID or necessarily the eventual application's PID. The child
			may already have exited and the PID may have been reused; this does not
			provide process supervision or an exit status.

			Replies are best-effort and do not block Alacritty on a nonreading client.
			Waiting for a reply times out after 10 seconds. A missing reply, including
			with an older running Alacritty, does not mean creation failed. The window
			may already have been created; do not automatically retry the command.

		*--hold*

			Remain open after child process exits.

	*OPTIONS*
		*--working-directory* _<WORKING_DIRECTORY>_

			Start the shell in the specified working directory.

		*-T, --title* _<TITLE>_

			Defines the window title.

			Default: _Alacritty_

		*--class* _<GENERAL>_ | _<GENERAL>_,_<INSTANCE>_

			Defines window class/app_id on X11/Wayland.

			Default: _Alacritty,Alacritty_

		*-o, --option* _<OPTION>..._

			Override configuration file options.

			Example: _alacritty msg create-window -o 'cursor.style="Beam"'_

		*-e, --command* _<COMMAND>..._

			Command and args to execute (must be last argument).

*config*

	Update the Alacritty configuration.

	*ARGS*
		*<CONFIG_OPTIONS>...*

			Configuration file options.

			Example: _alacritty msg config 'cursor.style="Beam"'_

	*FLAGS*
		*-r, --reset*

			Clear all runtime configuration changes.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the new config.

			Use _-1_ to apply this change to all windows.

			Default: _$ALACRITTY_WINDOW_ID_

*get-config*

	Read runtime Alacritty configuration.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the config request.

			Use _-1_ to get the global config.

			Default: _$ALACRITTY_WINDOW_ID_

# SEE ALSO

*alacritty*(1), *alacritty*(5), *alacritty-bindings*(5), *alacritty-escapes*(7)

# BUGS

Found a bug? Please report it at _https://github.com/alacritty/alacritty/issues_.

# MAINTAINERS

- Christian Duerr <contact@christianduerr.com>
- Kirill Chibisov <contact@kchibisov.com>
