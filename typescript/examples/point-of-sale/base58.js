const base58 = require('bs58');

const key = require(process.argv[2]);

console.log(base58.encode(key));
