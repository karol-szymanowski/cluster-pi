use cluster_common::crd::{NodeRole, PiNode};

pub struct CloudInitGenerator;

impl CloudInitGenerator {
    /// Generates dynamic Raspberry Pi ARM64 cmdline.txt with memory cgroups enabled.
    pub fn generate_cmdline(vip: &str, http_port: u16) -> String {
        format!(
            "console=serial0,115200 console=tty1 root=/dev/ram0 rdinit=/init cgroup_memory=1 cgroup_enable=memory ds=nocloud-net;s=http://{}:{}/cloud-init/ netboot=1",
            vip, http_port
        )
    }

    /// Dynamically renders cloud-init user-data YAML tailored to the requesting node's role.
    pub fn generate_user_data(node: &PiNode, vip: &str, k3s_token: &str) -> String {
        let role = &node.spec.desired_role;

        let hostname = node
            .spec
            .hostname
            .clone()
            .unwrap_or_else(|| format!("pi-{}", node.spec.hardware_serial));

        let is_server = matches!(role, NodeRole::Master | NodeRole::Seed);
        let k3s_service_name = if is_server {
            "k3s.service"
        } else {
            "k3s-agent.service"
        };

        let k3s_install_cmd = if is_server {
            format!(
                "curl -sfL https://get.k3s.io | K3S_URL=https://{}:6443 K3S_TOKEN={} INSTALL_K3S_EXEC=\"server --server https://{}:6443\" sh -",
                vip, k3s_token, vip
            )
        } else {
            format!(
                "curl -sfL https://get.k3s.io | K3S_URL=https://{}:6443 K3S_TOKEN={} sh -",
                vip, k3s_token
            )
        };

        let role_json = serde_json::json!({
            "role": role,
            "target_disk_id": node.spec.target_disk_id,
            "reformat_confirmed": node.spec.reformat_confirmed,
        });

        format!(
            r#"#cloud-config
hostname: {hostname}
manage_etc_hosts: true

write_files:
  - path: /etc/cluster-ldm/role.json
    permissions: '0644'
    content: |
      {role_json_formatted}

  - path: /etc/systemd/system/cluster-ldm.service
    permissions: '0644'
    content: |
      [Unit]
      Description=Pi Cluster Local Disk Manager
      After=local-fs-pre.target
      Before={k3s_service_name}
      [Service]
      Type=oneshot
      RemainAfterExit=yes
      ExecStart=/usr/local/bin/cluster-ldm provision --role-source=/etc/cluster-ldm/role.json
      ExecStartPost=/usr/local/bin/cluster-ldm ipc-serve --socket=/run/cluster-ldm.sock &
      [Install]
      WantedBy=multi-user.target

runcmd:
  - systemctl daemon-reload
  - systemctl enable --now cluster-ldm.service
  - {k3s_install_cmd}
"#,
            hostname = hostname,
            role_json_formatted = role_json,
            k3s_service_name = k3s_service_name,
            k3s_install_cmd = k3s_install_cmd
        )
    }

    /// Generates cloud-init meta-data YAML
    pub fn generate_meta_data(node: &PiNode) -> String {
        let hostname = node
            .spec
            .hostname
            .clone()
            .unwrap_or_else(|| format!("pi-{}", node.spec.hardware_serial));
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            node.spec.hardware_serial, hostname
        )
    }
}
