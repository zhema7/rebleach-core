@echo off
cd /d %~dp0
target\release\rebleach-client.exe --server ws://127.0.0.1:8088/ws --id devA --role controller --peer devB > logs_schA.txt 2>&1
