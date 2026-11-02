@echo off
cd /d %~dp0
set RUST_LOG=rebleach_client=debug
target\release\rebleach-client.exe --server ws://127.0.0.1:8088/ws --id devB --role controlled --fps 60 > logs_ctl.txt 2>&1
