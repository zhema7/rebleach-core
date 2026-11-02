@echo off
cd /d %~dp0
start "" /min target\release\rebleach-client.exe --server ws://127.0.0.1:8088/ws --id devB --role controlled --fps 15
