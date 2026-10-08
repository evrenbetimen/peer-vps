//! `autounattend.xml` for Windows installs from an ISO.
//!
//! Windows Setup looks for this file at the root of every removable drive; the
//! QEMU backend serves it from a small read-only USB stick. It only answers
//! the questions that block a VM install and leaves the rest (language, disk,
//! edition, product key) to the person at the screen:
//!
//! - **windowsPE**: sets the `LabConfig` keys so Windows 11 Setup skips its
//!   TPM 2.0, Secure Boot, RAM, storage and CPU checks, which a VM without
//!   swtpm and a secure-boot firmware cannot pass.
//! - **specialize**: names the computer.
//! - **oobeSystem**: creates the local administrator the node hands out
//!   (no Microsoft account, no network needed), signs it in once, and on that
//!   first sign-in turns on Remote Desktop and its firewall rules and loads
//!   virtio network drivers if a virtio-win disc is attached.

use super::Arch;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn autounattend(arch: Arch, computer: &str, user: &str, password: &str) -> String {
    let arch = match arch {
        Arch::X86_64 => "amd64",
        Arch::Aarch64 => "arm64",
    };
    let driver_dir = match arch {
        "arm64" => r"NetKVM\w11\ARM64",
        _ => r"NetKVM\w11\amd64",
    };
    let component = |name: &str, body: &str| {
        format!(
            r#"    <component name="{name}" processorArchitecture="{arch}" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
{body}
    </component>
"#
        )
    };
    let lab_config: String =
        ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck", "BypassStorageCheck", "BypassCPUCheck"]
            .iter()
            .enumerate()
            .map(|(i, key)| {
                format!(
                    r#"        <RunSynchronousCommand wcm:action="add">
          <Order>{}</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v {key} /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
"#,
                    i + 1
                )
            })
            .collect();
    let first_logon = [
        r#"reg add "HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server" /v fDenyTSConnections /t REG_DWORD /d 0 /f"#.to_owned(),
        // The rule group's resource id, so this works in every Windows language.
        r#"netsh advfirewall firewall set rule group="@FirewallAPI.dll,-28752" new enable=Yes"#.to_owned(),
        format!(
            r#"cmd /c for %d in (D E F G H I J) do if exist %d:\{driver_dir} pnputil /add-driver %d:\{driver_dir}\*.inf /install"#
        ),
    ]
    .iter()
    .enumerate()
    .map(|(i, cmd)| {
        format!(
            r#"        <SynchronousCommand wcm:action="add">
          <Order>{}</Order>
          <CommandLine>{}</CommandLine>
        </SynchronousCommand>
"#,
            i + 1,
            esc(cmd)
        )
    })
    .collect::<String>();
    let (user, password, computer) = (esc(user), esc(password), esc(computer));

    let pe =
        component("Microsoft-Windows-Setup", &format!("      <RunSynchronous>\n{lab_config}      </RunSynchronous>"));
    let specialize =
        component("Microsoft-Windows-Shell-Setup", &format!("      <ComputerName>{computer}</ComputerName>"));
    let oobe = component(
        "Microsoft-Windows-Shell-Setup",
        &format!(
            r#"      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <HideLocalAccountScreen>true</HideLocalAccountScreen>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Name>{user}</Name>
            <Group>Administrators</Group>
            <Password>
              <Value>{password}</Value>
              <PlainText>true</PlainText>
            </Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Enabled>true</Enabled>
        <LogonCount>1</LogonCount>
        <Username>{user}</Username>
        <Password>
          <Value>{password}</Value>
          <PlainText>true</PlainText>
        </Password>
      </AutoLogon>
      <FirstLogonCommands>
{first_logon}      </FirstLogonCommands>"#
        ),
    );
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- Written by PeerVPS for an ISO install; see crates/peervps-core/src/virtualization/qemu/unattend.rs -->
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
{pe}  </settings>
  <settings pass="specialize">
{specialize}  </settings>
  <settings pass="oobeSystem">
{oobe}  </settings>
</unattend>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_the_blocking_questions_for_the_guest_arch() {
        let xml = autounattend(Arch::Aarch64, "pv-1234", "peervps", "p<w&\"d");
        assert!(xml.contains(r#"processorArchitecture="arm64""#) && !xml.contains("amd64"));
        for key in ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck"] {
            assert!(xml.contains(&format!(r"LabConfig /v {key} /t REG_DWORD /d 1")), "{key}");
        }
        assert!(xml.contains("<ComputerName>pv-1234</ComputerName>"));
        assert!(xml.contains("<Name>peervps</Name>") && xml.contains("<Group>Administrators</Group>"));
        assert!(xml.contains("<Value>p&lt;w&amp;&quot;d</Value>"), "password is escaped");
        assert!(xml.contains("fDenyTSConnections") && xml.contains("-28752"));
        assert!(xml.contains(r"NetKVM\w11\ARM64"));
        let x64 = autounattend(Arch::X86_64, "pv", "u", "p");
        assert!(x64.contains(r#"processorArchitecture="amd64""#) && x64.contains(r"NetKVM\w11\amd64"));
        // Every opened element is closed: a cheap well-formedness check.
        for tag in ["unattend", "settings", "component", "RunSynchronous", "OOBE", "FirstLogonCommands", "AutoLogon"] {
            assert_eq!(
                x64.matches(&format!("<{tag}>")).count() + x64.matches(&format!("<{tag} ")).count(),
                x64.matches(&format!("</{tag}>")).count(),
                "{tag}"
            );
        }
    }
}
