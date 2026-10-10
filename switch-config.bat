@echo off
wsl -e sh -c "cd /mnt/c/Users/Hristo/modbus-ups-nut-master && python3 switch_config.py %*"
