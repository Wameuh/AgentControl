//! Windows : réparation des raccourcis (.lnk) sans `System.AppUserModel.ID`.
//!
//! Les pins taskbar créés avant l'AUMID explicite (#214 process, #253
//! installateurs) sont des .lnk SANS la propriété `System.AppUserModel.ID` :
//! Windows dérive alors leur identité du chemin de l'exe → après chaque
//! update l'icône épinglée ne groupe plus avec la fenêtre et « saute » en
//! doublon, indéfiniment. Aucun changement côté app ne peut réparer un
//! .lnk déjà épinglé — on le fait ici, une fois par démarrage, en posant
//! la propriété sur les .lnk qui ciblent notre exe : pins taskbar +
//! raccourcis bureau/menu Démarrer hérités d'installateurs anciens (un pin
//! créé depuis eux resterait cassé sinon).
//!
//! `WScript.Shell` ne sait pas écrire cette propriété (property store du
//! fichier, pas un champ IShellLink) — il faut `IPropertyStore` en COM,
//! sans equivalent typé dans windows-sys. On délègue donc à un script
//! PowerShell + `Add-Type` C# lancé en thread détaché : invisible pour
//! l'utilisateur, idempotent, journalisé dans client.log.

#![cfg(target_os = "windows")]

/// Lance la réparation en arrière-plan. `aumid` = `identifier` du bundle,
/// la même valeur que `set_app_user_model_id` et que les raccourcis posés
/// par les installateurs.
pub fn spawn_fix(aumid: &str) {
    let aumid = aumid.to_string();
    std::thread::spawn(move || {
        if let Err(e) = run(&aumid) {
            client_core::log(client_core::Level::Warn, format!("pinfix: {e}"));
        }
    });
}

/// Quote une valeur pour une chaîne littérale PowerShell ('...' avec ''
/// pour échapper une quote).
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn run(aumid: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let script = SCRIPT
        .replace("__EXE_PATH__", &ps_quote(&exe.to_string_lossy()))
        .replace("__AUMID__", &ps_quote(aumid));
    // -File plutôt que -Command : pas de quoting à doubler, et le script
    // peut être relu dans %TEMP% pour debug si besoin.
    let ps1 = std::env::temp_dir().join(format!("agentcontrol-pinfix-{}.ps1", std::process::id()));
    // BOM UTF-8 : Windows PowerShell 5.1 lit un .ps1 sans BOM en ANSI —
    // un chemin utilisateur accentué (ex. LOCALAPPDATA d'un compte
    // « René ») serait corrompu. Le script lui-même reste ASCII.
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(script.as_bytes());
    std::fs::write(&ps1, &bytes).map_err(|e| format!("write {ps1:?}: {e}"))?;
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&ps1)
        .output();
    let _ = std::fs::remove_file(&ps1);
    let out = out.map_err(|e| format!("spawn powershell: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if !line.is_empty() {
            client_core::log(client_core::Level::Info, format!("pinfix: {line}"));
        }
    }
    if !out.status.success() {
        return Err(format!("powershell exit {:?}", out.status.code()));
    }
    Ok(())
}

/// Script PowerShell : énumère les .lnk des dossiers à risque, ne touche
/// que ceux qui ciblent notre exe (chemin exact ou même nom de binaire —
/// un exe déplacé reste couvert), et pose `System.AppUserModel.ID` via
/// `IPropertyStore` quand il manque ou diffère. `__EXE_PATH__` /
/// `__AUMID__` sont remplacés côté Rust avant écriture du .ps1.
const SCRIPT: &str = r##"
$ErrorActionPreference = 'SilentlyContinue'
$ExePath = __EXE_PATH__
$Aumid   = __AUMID__
$ExeName = [IO.Path]::GetFileName($ExePath)

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class LnkAumid
{
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    public struct PROPERTYKEY
    {
        public Guid fmtid;
        public uint pid;
    }

    // PROPVARIANT : vt(2) + reserved(6) + union(8). On n'utilise que
    // VT_LPWSTR (31) : pwszVal est le premier membre de l'union, à
    // l'offset 8 — la disposition séquentielle ci-dessous le respecte
    // sur x86 comme x64/arm64.
    [StructLayout(LayoutKind.Sequential)]
    public struct PROPVARIANT
    {
        public ushort vt;
        public ushort wReserved1;
        public ushort wReserved2;
        public ushort wReserved3;
        public IntPtr pwszVal;
    }

    [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"),
     InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPropertyStore
    {
        [PreserveSig] int GetCount(out uint count);
        [PreserveSig] int GetAt(uint i, out PROPERTYKEY pkey);
        [PreserveSig] int GetValue(ref PROPERTYKEY pkey, out PROPVARIANT ppropvar);
        [PreserveSig] int SetValue(ref PROPERTYKEY pkey, ref PROPVARIANT ppropvar);
        [PreserveSig] int Commit();
    }

    [ComImport, Guid("43826D1E-E718-42EE-BC55-A1E261C37BFE"),
     InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItem
    {
        [PreserveSig] int BindToHandler(IntPtr pbc,
            [MarshalAs(UnmanagedType.LPStruct)] Guid bhid,
            ref Guid riid, out IntPtr ppv);
        [PreserveSig] int GetParent(out IShellItem ppsi);
        [PreserveSig] int GetDisplayName(uint sigdn,
            [MarshalAs(UnmanagedType.LPWStr)] out string ppszName);
        [PreserveSig] int GetAttributes(uint sfgaoMask, out uint psfgaoAttribs);
        [PreserveSig] int Compare(IShellItem psi, uint hint, out int piOrder);
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHCreateItemFromParsingName(string pszPath, IntPtr pbc,
        ref Guid riid, out IShellItem ppv);

    [DllImport("ole32.dll")]
    static extern void PropVariantClear(ref PROPVARIANT pvar);

    [DllImport("ole32.dll")]
    static extern int CoInitializeEx(IntPtr pvReserved, uint dwCoInit);

    static readonly Guid IID_IShellItem =
        new Guid("43826D1E-E718-42EE-BC55-A1E261C37BFE");
    static readonly Guid IID_IPropertyStore =
        new Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99");
    static readonly Guid BHID_PropertyStore =
        new Guid("28636AA6-953D-11D2-B5D6-0020AF19FE5D");
    // PKEY_AppUserModel_ID = {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, pid 5
    // (System.AppUserModel.ID — la propriété écrite par ShortcutProperty
    // des .wxs et SetLnkAppUserModelId du template NSIS).
    static readonly PROPERTYKEY PKEY_AppUserModel_ID = new PROPERTYKEY
    {
        fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"),
        pid = 5
    };

    // "ok" (déjà bon), "fixed" (propriété posée), ou "err:<raison>".
    public static string Ensure(string lnkPath, string aumid)
    {
        try
        {
            CoInitializeEx(IntPtr.Zero, 0x2); // COINIT_APARTMENTTHREADED

            IShellItem item;
            Guid iidItem = IID_IShellItem;
            int hr = SHCreateItemFromParsingName(
                lnkPath, IntPtr.Zero, ref iidItem, out item);
            if (hr != 0 || item == null)
                return "err:SHCreateItem 0x" + hr.ToString("x8");

            // IShellItem::BindToHandler(BHID_PropertyStore) — la voie
            // documentée pour obtenir l'IPropertyStore d'un fichier.
            IntPtr pps;
            Guid bhid = BHID_PropertyStore;
            Guid iidPs = IID_IPropertyStore;
            hr = item.BindToHandler(IntPtr.Zero, bhid, ref iidPs, out pps);
            if (hr != 0 || pps == IntPtr.Zero)
                return "err:BindToHandler 0x" + hr.ToString("x8");
            object unk = Marshal.GetObjectForIUnknown(pps);
            Marshal.Release(pps);
            IPropertyStore store = (IPropertyStore)unk;

            PROPERTYKEY key = PKEY_AppUserModel_ID;

            // Lecture : la propriété est absente (TYPE_E_ELEMENTNOTFOUND
            // = 0x8002802B) ou d'un autre type → on (ré)écrit.
            PROPVARIANT cur;
            hr = store.GetValue(ref key, out cur);
            if (hr == 0)
            {
                string s = null;
                if ((cur.vt == 31 || cur.vt == 8) && cur.pwszVal != IntPtr.Zero)
                    s = Marshal.PtrToStringUni(cur.pwszVal);
                PropVariantClear(ref cur);
                if (string.Equals(s, aumid, StringComparison.OrdinalIgnoreCase))
                    return "ok";
            }
            else if (hr != unchecked((int)0x8002802B))
            {
                return "err:GetValue 0x" + hr.ToString("x8");
            }

            IntPtr pstr = Marshal.StringToCoTaskMemUni(aumid);
            PROPVARIANT pv = new PROPVARIANT
            {
                vt = 31, // VT_LPWSTR
                pwszVal = pstr
            };
            hr = store.SetValue(ref key, ref pv);
            if (hr == 0)
                hr = store.Commit();
            Marshal.FreeCoTaskMem(pstr);
            return hr == 0 ? "fixed" : "err:SetValue 0x" + hr.ToString("x8");
        }
        catch (Exception e)
        {
            return "err:" + e.GetType().Name;
        }
    }
}
'@

$dirs = @(
    (Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar'),
    ([Environment]::GetFolderPath([Environment+SpecialFolder]::Desktop)),
    (Join-Path $env:PUBLIC 'Desktop'),
    (Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::StartMenu)) 'Programs')
)

$ws = New-Object -ComObject WScript.Shell
$scanned = 0; $ours = 0; $fixed = 0; $ok = 0; $err = 0

foreach ($d in $dirs) {
    Get-ChildItem -LiteralPath $d -Filter *.lnk -Recurse -File -ErrorAction SilentlyContinue |
        ForEach-Object {
            try {
                $scanned++
                $lnk = $ws.CreateShortcut($_.FullName)
                $target = $lnk.TargetPath
                $isOurs = $target -and
                          (($target -ieq $ExePath) -or
                           ([IO.Path]::GetFileName($target) -ieq $ExeName))
                if ($isOurs) {
                    $ours++
                    $r = [LnkAumid]::Ensure($_.FullName, $Aumid)
                    switch -Wildcard ($r) {
                        'ok'    { $ok++ }
                        'fixed' { $fixed++ }
                        default { $err++; Write-Output ("pinfix-err " + $_.FullName + " -> " + $r) }
                    }
                }
            } catch { }
        }
}
Write-Output ("pinfix: scanned=" + $scanned + " ours=" + $ours +
              " ok=" + $ok + " fixed=" + $fixed + " err=" + $err)
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Le script embarqué doit rester un here-string PowerShell valide :
    /// `'@` en début de ligne termine le Add-Type — on vérifie qu'il
    /// n'apparaît qu'une fois (la nôtre) et que les placeholders sont
    /// bien substitués à l'exécution.
    #[test]
    fn script_shape() {
        assert!(SCRIPT.contains("Add-Type -TypeDefinition @'"));
        assert_eq!(SCRIPT.matches("\n'@\n").count(), 1);
        assert!(SCRIPT.contains("__EXE_PATH__"));
        assert!(SCRIPT.contains("__AUMID__"));
        // Les placeholders remplacés ne laissent aucune trace.
        let filled = SCRIPT
            .replace(
                "__EXE_PATH__",
                &ps_quote("C:\\Program Files\\AgentControl\\agentcontrol.exe"),
            )
            .replace("__AUMID__", &ps_quote("dev.agentcontrol.app"));
        assert!(!filled.contains("__EXE_PATH__"));
        assert!(!filled.contains("__AUMID__"));
        assert!(filled.contains("'C:\\Program Files\\AgentControl\\agentcontrol.exe'"));
    }

    #[test]
    fn ps_quote_escapes_single_quotes() {
        assert_eq!(ps_quote("l'école"), "'l''école'");
        assert_eq!(ps_quote("simple"), "'simple'");
    }
}
