' Start hidden BEFORE a console is allocated, not after PowerShell starts.
Set shell = CreateObject("WScript.Shell")
Set files = CreateObject("Scripting.FileSystemObject")
folder = files.GetParentFolderName(WScript.ScriptFullName)
exe = shell.ExpandEnvironmentStrings("%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe")
command = """" & exe & """ -NoProfile -NonInteractive -ExecutionPolicy Bypass -File """ & folder & "\gui-task.ps1"" -Folder """ & folder & """"
WScript.Quit shell.Run(command, 0, True)
