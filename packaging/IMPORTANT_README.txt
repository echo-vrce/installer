Echo VRCE Installer
===================

KEEP BOTH FILES IN THE SAME FOLDER.

  echo-vrce-installer.exe   the window. This is the one to double-click.
  echo-vrce-cli.exe         the same program without a window, for a terminal
                            or a script.

They are not two programs. They share every line of code, and there are two
files only because a Windows program that opens a window cannot also print to
the console it was launched from.

Why they have to stay together
------------------------------

Most of what the installer does needs no special rights. When a step does need
administrator rights, such as installing into C:\Program Files or adding Echo
to Revive's app list, the window runs echo-vrce-cli.exe as the elevated worker
and watches it.

It looks for that file NEXT TO ITSELF and nowhere else. Move it, rename it, or
run the installer from a different folder, and those steps stop with:

  echo-vrce-cli.exe is not next to the installer. Both files belong in the
  same folder.

Everything else keeps working. Nothing is damaged. But the step you wanted
will not run until the two files are together again.

Where to put them
-----------------

Anywhere you can write to: Downloads, Desktop, a folder of your own, a USB
stick. Do not put them inside the game folder. The installer deletes and
rebuilds that folder when it reinstalls.

The first time you run it
-------------------------

Windows will show a blue SmartScreen box saying it protected your PC. That is
because the program is not code-signed, not because anything is wrong with it.
Click "More info", then "Run anyway".

If that makes you uneasy, that is a reasonable instinct. Check where you
downloaded it from first.

Source, and how to report a problem
-----------------------------------

  https://github.com/echo-vrce/installer

The installer keeps a log of every run at:

  %LOCALAPPDATA%\EchoVRCE\logs

Attach the newest one when you ask for help. There is also a Tools screen that
collects a support bundle off a headset, which answers most of the questions
anyone would otherwise have to ask you.
