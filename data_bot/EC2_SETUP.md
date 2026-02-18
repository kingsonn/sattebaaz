# EC2 Ubuntu VPS Setup Guide

## 1. Launch EC2 Instance
- AMI: **Ubuntu 22.04 LTS**
- Instance type: `t3.small` or larger (1 vCPU, 2GB RAM is enough)
- Security Group: open inbound **TCP port 8050** (for dashboard access)
- Key pair: download your `.pem` file

## 2. Connect via SSH
```bash
ssh -i your-key.pem ubuntu@<your-ec2-public-ip>
```

## 3. Install Dependencies
```bash
sudo apt update && sudo apt upgrade -y
sudo apt install -y python3 python3-pip python3-venv git
```

## 4. Upload Your Project
From your local machine (PowerShell):
```powershell
scp -i your-key.pem -r C:\Users\hanso\Projects\sattebaaz\data_bot ubuntu@<ec2-ip>:~/data_bot
```
Or use `rsync` if available:
```bash
rsync -avz -e "ssh -i your-key.pem" C:/Users/hanso/Projects/sattebaaz/data_bot/ ubuntu@<ec2-ip>:~/data_bot/
```

## 5. Set Up Python Virtual Environment
```bash
cd ~/data_bot
python3 -m venv venv
source venv/bin/activate
pip install -r requirements.txt
```

## 6. Test Run (make sure it works)
```bash
cd ~/data_bot
source venv/bin/activate
python app.py
# Visit http://<ec2-ip>:8050 in your browser
# Ctrl+C to stop
```

## 7. Create systemd Service (runs forever, survives SSH close + reboots)

Create the service file:
```bash
sudo nano /etc/systemd/system/databot.service
```

Paste this content:
```ini
[Unit]
Description=Polymarket BTC Data Collector
After=network.target

[Service]
Type=simple
User=ubuntu
WorkingDirectory=/home/ubuntu/data_bot
ExecStart=/home/ubuntu/data_bot/venv/bin/python app.py
Restart=always
RestartSec=5
StandardOutput=append:/home/ubuntu/data_bot/app.log
StandardError=append:/home/ubuntu/data_bot/app_err.log

[Install]
WantedBy=multi-user.target
```

Save and exit (`Ctrl+O`, `Enter`, `Ctrl+X`).

## 8. Enable and Start the Service
```bash
sudo systemctl daemon-reload
sudo systemctl enable databot      # auto-start on reboot
sudo systemctl start databot       # start now
sudo systemctl status databot      # check it's running
```

## 9. Useful Commands
```bash
# View live logs
tail -f ~/data_bot/app.log

# Stop the bot
sudo systemctl stop databot

# Restart after code changes
sudo systemctl restart databot

# Disable auto-start
sudo systemctl disable databot
```

## 10. Update Code on VPS
After making changes locally, re-upload and restart:
```powershell
# From local PowerShell
scp -i your-key.pem C:\Users\hanso\Projects\sattebaaz\data_bot\app.py ubuntu@<ec2-ip>:~/data_bot/
scp -i your-key.pem C:\Users\hanso\Projects\sattebaaz\data_bot\collector.py ubuntu@<ec2-ip>:~/data_bot/
scp -i your-key.pem C:\Users\hanso\Projects\sattebaaz\data_bot\templates\dashboard.html ubuntu@<ec2-ip>:~/data_bot/templates/
```
Then on the VPS:
```bash
sudo systemctl restart databot
```

## 11. Access Dashboard
Open in browser: `http://<your-ec2-public-ip>:8050`

> **Note:** The DB file (`btc_5m_data.db`) persists on the VPS. Your data is safe across restarts.
> If you want to copy your existing DB from local to VPS:
> ```powershell
> scp -i your-key.pem C:\Users\hanso\Projects\sattebaaz\data_bot\btc_5m_data.db ubuntu@<ec2-ip>:~/data_bot/
> ```
