searchState.loadedDescShard("iroh_dns_server", 0, "A DNS server and pkarr relay\nConfiguration for the server\nImplementation of a DNS name server for iroh node announces\nHTTP server part of iroh-dns-server\nMetrics support for the server\nThe main server which combines the DNS and HTTP(S) servers.\nShared state and store for the iroh-dns-server\nPkarr packet store used to resolve DNS queries.\nConfigure the bootstrap servers for mainline DHT …\nServer configuration\nUse custom bootstrap servers.\nUse the default bootstrap servers.\nThe config for the metrics server.\nThe config for the metrics server.\nOptionally set a custom address to bind to.\nSet custom bootstrap nodes.\nGet the data directory.\nDisable the metrics server.\nSet to true to disable the metrics server.\nConfig for the DNS server.\nSet to true to enable the mainline lookup.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nConfig for the HTTP server\nConfig for the HTTPS server\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nLoad the config from a file.\nConfig for the mainline lookup.\nConfig for the metrics server.\nGet the address where the metrics server should be bound, …\nGet the path to the store database file.\nDNS server settings\nState for serving DNS\nA DNS server that serves pkarr signed packets.\nA handle to the channel over which the response to a DNS …\nHandle a DNS request\nThe IPv4 or IPv6 address to bind the UDP DNS server. Uses …\nSOA record data for any authoritative DNS records\nDefault time to live for returned DNS records (TXT &amp; SOA)\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nGet the local address of the UDP/TCP socket.\nCreate a DNS server given some settings, a connection to …\nDomain used for serving the <code>_iroh_node.&lt;nodeid&gt;.&lt;origin&gt;</code> …\nThe port to serve a local UDP DNS server at\n<code>A</code> record to set for all origins\n<code>AAAA</code> record to set for all origins\n<code>NS</code> record to set for all origins\nWait for all tasks to complete.\nShutdown the server an wait for all tasks to complete.\nSpawn the server.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nThe mode how SSL certificates should be created.\nConfig for the HTTP server\nThe HTTP(S) server part of iroh-dns-server\nConfig for the HTTPS server\nACME with LetsEncrypt servers\nCerts are loaded from a the <code>cert_cache</code> path\nCreate self-signed certificates and store them in the …\nOptionally set a custom bind address (will use 0.0.0.0 if …\nOptionally set a custom bind address (will use 0.0.0.0 if …\nThe mode of SSL certificate creation\nDNS over HTTPS\nThe list of domains for which SSL certificates should be …\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nGet the bound address of the HTTP socket.\nGet the bound address of the HTTPS socket.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nLetsencrypt contact email address (required if using …\nWhether to use the letsenrypt production servers (only …\nRecord request metrics.\nPort to bind to\nPort to bind to\nWait for all tasks to complete.\nShutdown the server and wait for all tasks to complete.\nSpawn the server\nExtractors for DNS-over-HTTPS requests\nGET handler for resolving DoH queries\nPOST handler for resolvng DoH queries\nDNS Response\nA DNS packet encoding type\nA DNS request encoded in the body\nA DNS request encoded in the query string\napplication/dns-json\napplication/dns-message\nUsed to disable DNSSEC validation\nDesired content type. E.g. “application/dns-message” …\nWhether to return DNSSEC entries such as RRSIG, NSEC or …\nPrivacy setting for how your IP address is forwarded to …\nExposed to make it usable internally…\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nRecord name to look up, e.g. example.com\nSome url-safe random characters to pad your messages for …\nRecord type, e.g. A/AAAA/TXT, etc.\nWhether to provide answers for all records up to the root\nTurn this mime type to an <code>Accept</code> HTTP header value\nJSON representation of a DNS response See: …\nJSON representation of a DNS question\nJSON representation of a DNS record\nWhether the response was validated with DNSSEC\nThe answers to the request\nWhether the client asked to disable DNSSEC validation\nAn optional comment\nRecord data\nIP Address / scope prefix-length of the client See: …\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nCreate a new JSON response from a DNS message\nCreate a new JSON question from a DNS query\nCreate a new JSON record from a DNS record\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nFQDN with trailing dot\nFQDN with trailing dot\nThe questions that this request answers\nStandard DNS RR type\nWhether recursion was available\nWhether recursion was desired\nStandard DNS RR type\nStandard DNS response code\nWhether the response was truncated\nTime-to-live, in seconds\nContains the error value\nContains the success value\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nCreate a new <code>AppError</code>.\nSerialize/Deserializer for status codes.\nDeserialize StatusCodes.\nSerialize StatusCodes.\nCreate the default rate-limiting layer.\nThe mode how SSL certificates should be created.\nACME with LetsEncrypt servers\nCerts are loaded from a the <code>cert_cache</code> path\nCreate self-signed certificates and store them in the …\nTLS Certificate Authority acceptor.\nBuild the <code>TlsAcceptor</code> for this mode.\nReturns the argument unchanged.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nMetrics for iroh-dns-server\nReturns the argument unchanged.\nInit the metrics collection core.\nCalls <code>U::from(self)</code>.\nThe iroh-dns server.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nWait for all tasks to complete.\nSpawn the server and run until the <code>Ctrl-C</code> signal is …\nCancel the server tasks and wait for all tasks to complete.\nSpawn the server.\nThe shared app state.\nHandler for DNS requests\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nThe pkarr DNS store\nCache up to 1 million pkarr zones by default\nDefault TTL for DHT cache entries\nWhere a new pkarr packet comes from\nReceived via HTTPS relay PUT\nA store for pkarr signed packets.\nCache for explicitly added entries\nCache for DHT entries, this must have a finite TTL so we …\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nGet the latest signed packet for a pubkey.\nCreate an in-memory store.\nInsert a signed packet into the cache and the store.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCreate a new zone store.\nCreate a persistent store\nResolve a DNS query.\nConfigure a pkarr client for resolution of packets from …\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.")