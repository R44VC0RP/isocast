param([Parameter(Mandatory)][string]$Folder)
$ErrorActionPreference = 'Stop'
try {
    $r = Get-Content -Raw -LiteralPath "$Folder/request.json" | ConvertFrom-Json
    Add-Type -Path "$Folder/gui-native.cs"
    [GuiNative]::DpiAware()
    Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes,System.Windows.Forms,System.Drawing
    $A = [Windows.Automation.AutomationElement]
    $script:desk = $r.host -eq 'desk'
    $script:session = (Get-Process -Id $PID).SessionId
    if ($script:session -ne $r.session -or $script:session -eq 0) { throw 'Interactive session changed or no user is logged on' }
    $script:initialIdle = [GuiNative]::IdleSeconds()

    function Guard {
        [GuiNative]::Unlocked()
        if ($script:desk -and -not $r.force_active -and [GuiNative]::IdleSeconds() -lt 120) {
            throw "Ryan appears active: last input $([math]::Round([GuiNative]::IdleSeconds(),1))s ago (minimum 120s). Refusing; --force-active requires his explicit authorization."
        }
    }
    function AllowedPath([string]$path) {
        if (-not $path) { return $false }
        $path = $path.Replace('/', '\') -replace '^\\\\\?\\', ''
        $path = [IO.Path]::GetFullPath($path)
        $name = [IO.Path]::GetFileName($path)
        $own = $name -match '^(isocast|isocast-ui|isocastd|isocast-capture)\.exe$'
        $install = $name -match '^Isocast[-_].*\.exe$'
        $uninstall = $name -match '^unins\d+\.exe$'
        $dev = $path.StartsWith('C:\isocast-dev\', [StringComparison]::OrdinalIgnoreCase)
        $installed = $path.StartsWith("$env:LOCALAPPDATA\Programs\Isocast\", [StringComparison]::OrdinalIgnoreCase) -or $path.StartsWith('C:\Program Files\Isocast\', [StringComparison]::OrdinalIgnoreCase)
        $update = $path -match ('^' + [regex]::Escape($env:LOCALAPPDATA) + '\\Isocast-Update-[0-9a-f-]{36}\\Isocast-update\.exe$')
        return ($own -and ($dev -or $installed)) -or ($install -and $dev) -or ($uninstall -and $installed) -or $update
    }
    function AllowedProcess([int]$processId, [int]$level=0) {
        $p = Get-Process -Id $processId -ErrorAction SilentlyContinue
        if (-not $p -or $p.SessionId -ne $script:session) { return $false }
        # These apps are forbidden even on the otherwise unrestricted VM.
        if ($p.ProcessName -match '^(camera-ndi-bridge|chatui|obs)($|[-_0-9])') { return $false }
        if (-not $script:desk) { return $true }
        if (AllowedPath $p.Path) { return $true }
        # Inno's extracted .tmp child is allowed only with Isocast product
        # metadata AND a still-live allowed installer ancestor. No explorer,
        # rundll32, PowerShell, cmd or arbitrary child-process blanket exception.
        if ($level -lt 4 -and $p.Path -and $p.Path -match '\\is-[^\\]+\.tmp\\[^\\]+\.tmp$' -and
            $p.MainModule.FileVersionInfo.ProductName -eq 'Isocast') {
            $info = Get-CimInstance Win32_Process -Filter "ProcessId=$processId"
            $parent = Get-Process -Id $info.ParentProcessId -ErrorAction SilentlyContinue
            if ($parent -and $parent.StartTime -le $p.StartTime) { return AllowedProcess $parent.Id ($level + 1) }
        }
        return $false
    }
    function AllowedWindow([IntPtr]$window) {
        # Common file/message dialogs normally live in the application's own
        # process and are covered here; unrelated system windows stay denied.
        return AllowedProcess ([GuiNative]::ProcessId($window))
    }
    function RequireWindow([IntPtr]$window) {
        if ($window -eq [IntPtr]::Zero -or -not (AllowedWindow $window)) { throw 'Window refused: process is outside the Isocast allow-list (or is a protected application)' }
    }
    function Rect($bounds) {
        if ($bounds.IsEmpty -or [double]::IsInfinity($bounds.Left) -or [double]::IsInfinity($bounds.Top)) { return $null }
        return @{x=[int]$bounds.Left;y=[int]$bounds.Top;width=[int]($bounds.Right-$bounds.Left);height=[int]($bounds.Bottom-$bounds.Top)}
    }
    function WindowInfo([IntPtr]$window) {
        $p = Get-Process -Id ([GuiNative]::ProcessId($window)) -ErrorAction Stop
        return @{handle=$window.ToInt64();title=[GuiNative]::Title($window);process=$p.ProcessName;pid=$p.Id;rect=(Rect ([GuiNative]::Bounds($window)));minimized=[GuiNative]::IsIconic($window)}
    }
    function Windows {
        foreach ($w in [GuiNative]::Windows()) {
            if (AllowedWindow $w) { WindowInfo $w }
        }
    }
    function SelectWindow {
        $all = @([GuiNative]::Windows() | Where-Object {
            $p = Get-Process -Id ([GuiNative]::ProcessId($_)) -ErrorAction SilentlyContinue
            $p -and $p.SessionId -eq $script:session -and
                (-not $r.window -or [GuiNative]::Title($_) -eq $r.window) -and
                (-not $r.process -or $p.ProcessName -eq ($r.process -replace '\.exe$', ''))
        })
        if (-not $r.window -and -not $r.process) {
            $w = [GuiNative]::GetForegroundWindow()
            RequireWindow $w
            return $w
        }
        # Explicit selection of a forbidden window is an error, not an empty tree.
        foreach ($w in $all) { RequireWindow $w }
        if ($all.Count -eq 0) { throw 'No matching visible window' }
        if ($all.Count -gt 1) {
            $foreground = [GuiNative]::GetForegroundWindow()
            if ($all -contains $foreground) { return $foreground }
            throw 'Multiple matching windows: specify an exact --window title'
        }
        return $all[0]
    }
    function FocusWindow([IntPtr]$w) {
        Guard
        RequireWindow $w
        if ([GuiNative]::IsIconic($w)) { [void][GuiNative]::ShowWindow($w, 9) }
        if ([GuiNative]::GetForegroundWindow() -ne $w) {
            [void][GuiNative]::SetForegroundWindow($w)
            Start-Sleep -Milliseconds 150
        }
        if ([GuiNative]::GetForegroundWindow() -ne $w) { throw 'Windows refused foreground focus; input was not sent' }
    }
    function Foreground {
        Guard
        $w = [GuiNative]::GetForegroundWindow()
        RequireWindow $w
        return $w
    }
    function Elements([IntPtr]$w, [string]$name) {
        $root = $A::FromHandle($w)
        $condition = [Windows.Automation.PropertyCondition]::new($A::NameProperty, $name)
        $root.FindAll([Windows.Automation.TreeScope]::Subtree, $condition) | Where-Object { -not $_.Current.IsOffscreen }
    }
    function ClickPoint([int]$x, [int]$y, [IntPtr]$expected) {
        Guard
        $hit = [GuiNative]::GetAncestor([GuiNative]::WindowFromPoint([GuiNative+Point]::new($x,$y)), 2)
        RequireWindow $hit
        if ($expected -ne [IntPtr]::Zero -and $hit -ne $expected) { throw 'Click point is covered by another window or outside the selected window' }
        FocusWindow $hit
        [void][GuiNative]::SetCursorPos($x,$y)
        # Recheck immediately before down; do not redirect a click after focus changed.
        if ([GuiNative]::GetForegroundWindow() -ne $hit) { throw 'Foreground changed before click' }
        $down = if ($r.right) { 8 } else { 2 }
        $up = if ($r.right) { 16 } else { 4 }
        $count = if ($r.double) { 2 } else { 1 }
        for ($i=0; $i -lt $count; $i++) {
            [GuiNative]::Unlocked()
            $point = [Windows.Forms.Cursor]::Position
            $currentHit = [GuiNative]::GetAncestor([GuiNative]::WindowFromPoint([GuiNative+Point]::new($point.X,$point.Y)),2)
            if ($point.X -ne $x -or $point.Y -ne $y -or $currentHit -ne $hit -or [GuiNative]::GetForegroundWindow() -ne $hit) {
                throw 'Pointer/window changed before click; remaining input was not sent'
            }
            RequireWindow $hit
            [GuiNative]::MouseEvent($down,0)
            [GuiNative]::MouseEvent($up,0)
            if ($count -eq 2) { Start-Sleep -Milliseconds 60 }
        }
    }
    $script:nodes = 0
    function TreeNode($element, [int]$depth) {
        $script:nodes++
        $c = $element.Current
        $node = [ordered]@{name=$c.Name;type=($c.ControlType.ProgrammaticName -replace '^ControlType\.','');automation_id=$c.AutomationId;rect=(Rect $c.BoundingRectangle);enabled=$c.IsEnabled;focused=$c.HasKeyboardFocus;offscreen=$c.IsOffscreen}
        $pattern = $null
        if ($c.IsPassword) { $node.value = '[password omitted]' }
        elseif ($element.TryGetCurrentPattern([Windows.Automation.ValuePattern]::Pattern,[ref]$pattern)) { $node.value = $pattern.Current.Value }
        if ($element.TryGetCurrentPattern([Windows.Automation.TogglePattern]::Pattern,[ref]$pattern)) { $node.checked = $pattern.Current.ToggleState.ToString() }
        $node.children = @()
        if ($depth -gt 0 -and $script:nodes -lt 2000) {
            $walker = [Windows.Automation.TreeWalker]::ControlViewWalker
            $child = $walker.GetFirstChild($element)
            while ($child -and $script:nodes -lt 2000) {
                $node.children += TreeNode $child ($depth-1)
                $child = $walker.GetNextSibling($child)
            }
        }
        return $node
    }
    Guard
    $result = @{ok=$true;command=$r.command;session=$script:session;idle_seconds=$script:initialIdle}
    switch ($r.command) {
        'windows' { $result.windows = @(Windows); $result.filtered = $script:desk }
        'screenshot' {
            $w = [IntPtr]::Zero
            if ($r.window -or $r.process) {
                $w = SelectWindow
                if ([GuiNative]::IsIconic($w)) { throw 'Cannot screenshot a minimized window; no focus-changing restore is performed' }
                $bounds = [GuiNative]::Bounds($w)
                $rect = Rect $bounds
                $result.window = WindowInfo $w
            } else {
                if ($script:desk) { throw 'Full-desktop screenshots are forbidden on desk; select an Isocast --window or --process' }
                $rect = Rect ([Windows.Forms.SystemInformation]::VirtualScreen)
            }
            Guard
            $bitmap = [Drawing.Bitmap]::new($rect.width,$rect.height)
            $graphics = [Drawing.Graphics]::FromImage($bitmap)
            try {
                if ($w -ne [IntPtr]::Zero) {
                    # Print only this HWND, never unrelated windows overlapping it.
                    RequireWindow $w
                    $dc = $graphics.GetHdc()
                    try { if (-not [GuiNative]::PrintWindow($w,$dc,2)) { throw 'PrintWindow failed; no unsafe full-screen fallback was attempted' } }
                    finally { $graphics.ReleaseHdc($dc) }
                    $result.capture = 'PrintWindow (GPU-only surfaces may be blank)'
                } else {
                    $graphics.CopyFromScreen($rect.x,$rect.y,0,0,[Drawing.Size]::new($rect.width,$rect.height))
                    $result.capture = 'virtual screen'
                }
                $bitmap.Save("$Folder/screenshot.png",[Drawing.Imaging.ImageFormat]::Png)
            } finally { $graphics.Dispose(); $bitmap.Dispose() }
            $result.rect = $rect
            $result.coordinates = 'PNG-local pixels; click --at uses the same selector and adds rect.x/y'
        }
        'tree' {
            $w = SelectWindow
            $result.window = WindowInfo $w
            $result.tree = TreeNode ($A::FromHandle($w)) $r.depth
            $result.nodes = $script:nodes
            $result.truncated = $script:nodes -ge 2000
            $result.sparse = $script:nodes -le 2
            if ($result.sparse) { $result.warning = 'Sparse UI Automation tree; Slint/custom controls may expose limited accessibility. Inspect a screenshot and use scoped pixel coordinates.' }
        }
        'click' {
            $w = if ($r.window -or $r.process -or $r.name) { SelectWindow } else { [IntPtr]::Zero }
            if ($r.name) {
                $matches = @(Elements $w $r.name | Where-Object { -not $r.type -or $_.Current.ControlType.ProgrammaticName -eq "ControlType.$($r.type)" })
                if ($matches.Count -ne 1) { throw "Expected one on-screen UIA match; found $($matches.Count). Use --type and a specific window." }
                $e = $matches[0]
                if (-not $e.Current.IsEnabled) { throw 'UIA element is disabled' }
                if (-not (AllowedProcess $e.Current.ProcessId)) { throw 'UIA element belongs to a forbidden process' }
                $pattern = $null
                Guard
                if (-not $r.double -and -not $r.right -and $e.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern,[ref]$pattern)) {
                    $pattern.Invoke()
                    $result.method = 'Invoke'
                } else {
                    FocusWindow $w
                    $b = $e.Current.BoundingRectangle
                    if ($b.IsEmpty -or $b.Width -le 0 -or $b.Height -le 0) { throw 'Element has no clickable rectangle' }
                    ClickPoint ([int]($b.X+$b.Width/2)) ([int]($b.Y+$b.Height/2)) $w
                    $result.method = 'rect centre'
                }
            } else {
                $b = if ($w -ne [IntPtr]::Zero) { Rect ([GuiNative]::Bounds($w)) } else { Rect ([Windows.Forms.SystemInformation]::VirtualScreen) }
                if ($r.at[0] -lt 0 -or $r.at[1] -lt 0 -or $r.at[0] -ge $b.width -or $r.at[1] -ge $b.height) { throw 'Coordinates are outside screenshot bounds' }
                ClickPoint ($b.x+$r.at[0]) ($b.y+$r.at[1]) $w
                $result.method = 'pixels'
            }
        }
        'type' {
            $w = Foreground
            $result.window = WindowInfo $w
            foreach ($c in $r.text.ToCharArray()) {
                [GuiNative]::Unlocked()
                if ([GuiNative]::GetForegroundWindow() -ne $w) { throw 'Foreground changed while typing; remaining text was not sent' }
                [GuiNative]::Character($c)
            }
            $result.characters = $r.text.Length
        }
        'key' {
            $w = Foreground
            $result.window = WindowInfo $w
            $parts = $r.combo.ToLowerInvariant().Split('+')
            $keys = @{ctrl=17;control=17;alt=18;shift=16;win=91;enter=13;return=13;tab=9;escape=27;esc=27;space=32;backspace=8;delete=46;insert=45;home=36;end=35;pageup=33;pagedown=34;left=37;up=38;right=39;down=40}
            $codes = @()
            foreach ($part in $parts) {
                if ($keys.ContainsKey($part)) { $codes += $keys[$part] }
                elseif ($part -match '^[a-z0-9]$') { $codes += [int][char]$part.ToUpperInvariant() }
                elseif ($part -match '^f([1-9]|1[0-2])$') { $codes += 111+[int]$Matches[1] }
                else { throw "Unknown key '$part': use ctrl/alt/shift + letter, digit, F1..F12 or enter/tab/escape/arrows/home/end/pageup/pagedown/delete/backspace/space" }
            }
            if ($script:desk -and (($codes -contains 91) -or (($codes -contains 18) -and (($codes -contains 9) -or ($codes -contains 27) -or ($codes -contains 32))) -or (($codes -contains 17) -and (($codes -contains 27) -or ($codes -contains 46))))) { throw 'System/desktop-switching hotkeys are forbidden on desk' }
            $pressed = @()
            try {
                foreach ($code in $codes) {
                    [GuiNative]::Unlocked()
                    if ([GuiNative]::GetForegroundWindow() -ne $w) { throw 'Foreground changed before key press' }
                    [GuiNative]::Key($code,$false)
                    $pressed += $code
                }
            } finally { [array]::Reverse($pressed); foreach ($code in $pressed) { [GuiNative]::Key($code,$true) } }
        }
        'scroll' {
            $w = Foreground
            $result.window = WindowInfo $w
            $point = [Windows.Forms.Cursor]::Position
            $hit = [GuiNative]::GetAncestor([GuiNative]::WindowFromPoint([GuiNative+Point]::new($point.X,$point.Y)),2)
            RequireWindow $hit
            if ($hit -ne $w) { throw 'Scroll refused: pointer is outside the foreground window' }
            [GuiNative]::MouseEvent(0x800,120*$r.amount)
        }
        'wait-for' {
            $deadline = (Get-Date).AddSeconds($r.timeout)
            $found = $null
            do {
                Guard
                if ($r.process) {
                    $candidates = @(Get-Process -Name ($r.process -replace '\.exe$','') -ErrorAction SilentlyContinue | Where-Object SessionId -eq $script:session)
                    foreach ($p in $candidates) {
                        if (-not (AllowedProcess $p.Id)) { throw 'Process refused: outside the Isocast allow-list (or protected)' }
                        $found = @{pid=$p.Id;process=$p.ProcessName}
                        break
                    }
                } else {
                    foreach ($w in [GuiNative]::Windows()) {
                        if ($r.window -and [GuiNative]::Title($w) -eq $r.window) { RequireWindow $w; $found = WindowInfo $w; break }
                        if ($r.text -and (AllowedWindow $w) -and @(Elements $w $r.text).Count -gt 0) { $found = WindowInfo $w; break }
                    }
                }
                if (-not $found) { Start-Sleep -Milliseconds 300 }
            } while (-not $found -and (Get-Date) -lt $deadline)
            if (-not $found) { throw "wait-for timed out after $($r.timeout)s" }
            $result.match = $found
        }
        'launch' {
            $path = [Environment]::ExpandEnvironmentVariables($r.path)
            if (-not [IO.Path]::IsPathRooted($path) -or -not (Test-Path -LiteralPath $path -PathType Leaf)) { throw 'launch requires an existing absolute .exe or .lnk path' }
            $args = $r.args
            $directory = [IO.Path]::GetDirectoryName($path)
            if ([IO.Path]::GetExtension($path) -eq '.lnk') {
                $shell = New-Object -ComObject WScript.Shell
                $link = $shell.CreateShortcut($path)
                if ($args) { throw 'Do not override shortcut arguments; launch the executable explicitly instead' }
                $path = $link.TargetPath
                $args = $link.Arguments
                $directory = if ($link.WorkingDirectory) { $link.WorkingDirectory } else { [IO.Path]::GetDirectoryName($path) }
            }
            if ([IO.Path]::GetExtension($path) -ne '.exe') { throw 'Only .exe or .lnk launch targets are supported' }
            if ([IO.Path]::GetFileNameWithoutExtension($path) -match '^(camera-ndi-bridge|chatui|obs)($|[-_0-9])') { throw 'Protected application; launch refused' }
            if ($script:desk -and -not (AllowedPath $path)) { throw 'Launch refused: executable is outside the Isocast allow-list' }
            Guard
            # Get the RUNNING desktop's dispatch, not the in-process
            # Shell.Application.ShellExecute (which would inherit our TEMP/job).
            # SWC_DESKTOP=8, SWFO_NEEDDISPATCH=1, CSIDL_DESKTOP=0.
            $shell = New-Object -ComObject Shell.Application
            $desktopHwnd = 0
            $desktop = $shell.Windows().FindWindowSW(0,$null,8,[ref]$desktopHwnd,1)
            if (-not $desktop -or -not $desktopHwnd) { throw 'Interactive Explorer desktop broker unavailable; launch was not attempted' }
            $directory = [Environment]::ExpandEnvironmentVariables($directory)
            $desktop.Document.Application.ShellExecute($path,$args,$directory,'open',1)
            $result.note = 'Launch handed to the interactive shell; use wait-for to confirm startup'
            $result.path = $path
        }
        'close' {
            # Winit/Slint also expose tiny untitled message-pump HWNDs. Closing
            # those is not a user close request and can strand the application.
            $all = @([GuiNative]::Windows() | Where-Object {
                $p = Get-Process -Id ([GuiNative]::ProcessId($_)) -ErrorAction SilentlyContinue
                $p -and $p.ProcessName -eq ($r.process -replace '\.exe$','') -and [GuiNative]::Title($_)
            })
            if (-not $all.Count) { throw 'No matching visible titled windows to close' }
            foreach ($w in $all) { RequireWindow $w }
            foreach ($w in $all) {
                Guard
                if (-not [GuiNative]::PostMessage($w,0x10,[IntPtr]::Zero,[IntPtr]::Zero)) { throw 'WM_CLOSE was refused' }
            }
            $result.close_requested = $all.Count
            $result.note = 'WM_CLOSE sent; no force kill. Unsaved-changes dialogs may remain; use wait-for/windows to inspect.'
        }
        default { throw 'Unknown GUI command' }
    }
    $result | ConvertTo-Json -Depth 50 -Compress | Set-Content -LiteralPath "$Folder/result.json" -Encoding UTF8
} catch {
    @{ok=$false;error=$_.Exception.Message;location=$_.InvocationInfo.PositionMessage} | ConvertTo-Json -Compress | Set-Content -LiteralPath "$Folder/result.json" -Encoding UTF8
    exit 1
}
