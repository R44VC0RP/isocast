using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class GuiNative {
    public delegate bool EnumProc(IntPtr window, IntPtr data);
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct Point { public int X, Y; public Point(int x, int y) { X=x; Y=y; } }
    [StructLayout(LayoutKind.Sequential)] struct LastInput { public uint Size, Time; }
    [StructLayout(LayoutKind.Sequential)] struct Keyboard { public ushort Key, Scan; public uint Flags, Time; public UIntPtr Extra; }
    [StructLayout(LayoutKind.Sequential)] struct Mouse { public int X, Y; public uint Data, Flags, Time; public UIntPtr Extra; }
    [StructLayout(LayoutKind.Explicit)] struct Union {
        [FieldOffset(0)] public Keyboard Keyboard;
        [FieldOffset(0)] public Mouse Mouse;
    }
    [StructLayout(LayoutKind.Sequential)] struct Input { public uint Type; public Union Data; }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback, IntPtr data);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr window);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr window, StringBuilder text, int length);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr window, uint flags);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr window, uint command);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(Point point);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr window, int command);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);
    [DllImport("user32.dll", SetLastError=true)] static extern uint SendInput(uint count, Input[] input, int size);
    [DllImport("user32.dll", SetLastError=true)] static extern bool GetLastInputInfo(ref LastInput input);
    [DllImport("user32.dll", SetLastError=true)] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool GetUserObjectInformation(IntPtr handle, int index, StringBuilder value, int size, out int needed);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
    public static void DpiAware() {
        try { if (SetProcessDpiAwarenessContext(new IntPtr(-4))) return; } catch (EntryPointNotFoundException) { }
        if (!SetProcessDPIAware()) throw new InvalidOperationException("Cannot enable real-pixel DPI awareness");
    }
    public static void Unlocked() {
        IntPtr d = OpenInputDesktop(0, false, 1);
        if (d == IntPtr.Zero) throw new InvalidOperationException("Screen is locked or input desktop is unavailable; unlock is required");
        try {
            var name = new StringBuilder(256); int needed;
            if (!GetUserObjectInformation(d, 2, name, 512, out needed) || name.ToString() != "Default")
                throw new InvalidOperationException("Screen is locked or a secure desktop is active; unlock is required (UAC is unsupported)");
        } finally { CloseDesktop(d); }
    }
    public static double IdleSeconds() {
        var input = new LastInput(); input.Size = (uint)Marshal.SizeOf(input);
        if (!GetLastInputInfo(ref input)) throw new Win32Exception();
        return unchecked((uint)Environment.TickCount - input.Time) / 1000.0;
    }
    public static IntPtr[] Windows() {
        var result = new List<IntPtr>();
        EnumWindows((window, data) => { if (IsWindowVisible(window)) result.Add(window); return true; }, IntPtr.Zero);
        return result.ToArray();
    }
    public static int ProcessId(IntPtr window) { uint pid; GetWindowThreadProcessId(window, out pid); return (int)pid; }
    public static string Title(IntPtr window) { var s=new StringBuilder(4096); GetWindowText(window,s,s.Capacity); return s.ToString(); }
    public static Rect Bounds(IntPtr window) { Rect r; if (!GetWindowRect(window,out r)) throw new Win32Exception(); return r; }
    static void Send(Input input) {
        if (SendInput(1, new[] { input }, Marshal.SizeOf(typeof(Input))) != 1)
            throw new InvalidOperationException("SendInput was refused (elevation/UIPI or desktop changed)");
    }
    public static void Key(ushort key, bool up) {
        var input = new Input { Type=1 }; input.Data.Keyboard.Key=key; input.Data.Keyboard.Flags=up ? 2u : 0u; Send(input);
    }
    public static void Character(char c) {
        var input = new Input { Type=1 }; input.Data.Keyboard.Scan=c; input.Data.Keyboard.Flags=4; Send(input);
        input.Data.Keyboard.Flags=6; Send(input);
    }
    public static void MouseEvent(uint flags, int data) {
        var input = new Input { Type=0 }; input.Data.Mouse.Flags=flags; input.Data.Mouse.Data=unchecked((uint)data); Send(input);
    }
}
