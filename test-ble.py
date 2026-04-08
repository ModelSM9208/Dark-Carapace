import asyncio
from bleak import BleakScanner, BleakClient

async def read():
    print('Scanning for DarkCarapace...')
    device = await BleakScanner.find_device_by_name('DarkCarapace', timeout=10)
    if not device:
        print('Not found — is the board still advertising?')
        return
    print(f'Found: {device.address}')
    async with BleakClient(device) as client:
        val = await client.read_gatt_char('937312e0-2354-11eb-9f10-fbc30a62cf39')
        print('Read:', val.decode())

asyncio.run(read())
